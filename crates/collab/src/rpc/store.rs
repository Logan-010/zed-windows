use crate::db::{self, ChannelId, ProjectId, UserId};
use anyhow::{anyhow, Result};
use collections::{btree_map, hash_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet};
use rpc::{proto, ConnectionId, Receipt};
use serde::Serialize;
use std::{mem, path::PathBuf, str, time::Duration};
use time::OffsetDateTime;
use tracing::instrument;
use util::post_inc;

pub type RoomId = u64;

#[derive(Default, Serialize)]
pub struct Store {
    connections: BTreeMap<ConnectionId, ConnectionState>,
    connections_by_user_id: BTreeMap<UserId, UserConnectionState>,
    next_room_id: RoomId,
    rooms: BTreeMap<RoomId, proto::Room>,
    projects: BTreeMap<ProjectId, Project>,
    #[serde(skip)]
    channels: BTreeMap<ChannelId, Channel>,
}

#[derive(Default, Serialize)]
struct UserConnectionState {
    connection_ids: HashSet<ConnectionId>,
    room: Option<RoomState>,
}

#[derive(Serialize)]
struct ConnectionState {
    user_id: UserId,
    admin: bool,
    projects: BTreeSet<ProjectId>,
    requested_projects: HashSet<ProjectId>,
    channels: HashSet<ChannelId>,
}

#[derive(Copy, Clone, Eq, PartialEq, Serialize)]
enum RoomState {
    Joined,
    Calling { room_id: RoomId },
}

#[derive(Serialize)]
pub struct Project {
    pub online: bool,
    pub host_connection_id: ConnectionId,
    pub host: Collaborator,
    pub guests: HashMap<ConnectionId, Collaborator>,
    #[serde(skip)]
    pub join_requests: HashMap<UserId, Vec<Receipt<proto::JoinProject>>>,
    pub active_replica_ids: HashSet<ReplicaId>,
    pub worktrees: BTreeMap<u64, Worktree>,
    pub language_servers: Vec<proto::LanguageServer>,
}

#[derive(Serialize)]
pub struct Collaborator {
    pub replica_id: ReplicaId,
    pub user_id: UserId,
    #[serde(skip)]
    pub last_activity: Option<OffsetDateTime>,
    pub admin: bool,
}

#[derive(Default, Serialize)]
pub struct Worktree {
    pub root_name: String,
    pub visible: bool,
    #[serde(skip)]
    pub entries: BTreeMap<u64, proto::Entry>,
    #[serde(skip)]
    pub diagnostic_summaries: BTreeMap<PathBuf, proto::DiagnosticSummary>,
    pub scan_id: u64,
    pub is_complete: bool,
}

#[derive(Default)]
pub struct Channel {
    pub connection_ids: HashSet<ConnectionId>,
}

pub type ReplicaId = u16;

#[derive(Default)]
pub struct RemovedConnectionState {
    pub user_id: UserId,
    pub hosted_projects: HashMap<ProjectId, Project>,
    pub guest_project_ids: HashSet<ProjectId>,
    pub contact_ids: HashSet<UserId>,
}

pub struct LeftProject {
    pub host_user_id: UserId,
    pub host_connection_id: ConnectionId,
    pub connection_ids: Vec<ConnectionId>,
    pub remove_collaborator: bool,
    pub cancel_request: Option<UserId>,
    pub unshare: bool,
}

pub struct UnsharedProject {
    pub guests: HashMap<ConnectionId, Collaborator>,
    pub pending_join_requests: HashMap<UserId, Vec<Receipt<proto::JoinProject>>>,
}

#[derive(Copy, Clone)]
pub struct Metrics {
    pub connections: usize,
    pub registered_projects: usize,
    pub active_projects: usize,
    pub shared_projects: usize,
}

impl Store {
    pub fn metrics(&self) -> Metrics {
        const ACTIVE_PROJECT_TIMEOUT: Duration = Duration::from_secs(60);
        let active_window_start = OffsetDateTime::now_utc() - ACTIVE_PROJECT_TIMEOUT;

        let connections = self.connections.values().filter(|c| !c.admin).count();
        let mut registered_projects = 0;
        let mut active_projects = 0;
        let mut shared_projects = 0;
        for project in self.projects.values() {
            if let Some(connection) = self.connections.get(&project.host_connection_id) {
                if !connection.admin {
                    registered_projects += 1;
                    if project.is_active_since(active_window_start) {
                        active_projects += 1;
                        if !project.guests.is_empty() {
                            shared_projects += 1;
                        }
                    }
                }
            }
        }

        Metrics {
            connections,
            registered_projects,
            active_projects,
            shared_projects,
        }
    }

    #[instrument(skip(self))]
    pub fn add_connection(&mut self, connection_id: ConnectionId, user_id: UserId, admin: bool) {
        self.connections.insert(
            connection_id,
            ConnectionState {
                user_id,
                admin,
                projects: Default::default(),
                requested_projects: Default::default(),
                channels: Default::default(),
            },
        );
        self.connections_by_user_id
            .entry(user_id)
            .or_default()
            .connection_ids
            .insert(connection_id);
    }

    #[instrument(skip(self))]
    pub fn remove_connection(
        &mut self,
        connection_id: ConnectionId,
    ) -> Result<RemovedConnectionState> {
        let connection = self
            .connections
            .get_mut(&connection_id)
            .ok_or_else(|| anyhow!("no such connection"))?;

        let user_id = connection.user_id;
        let connection_projects = mem::take(&mut connection.projects);
        let connection_channels = mem::take(&mut connection.channels);

        let mut result = RemovedConnectionState {
            user_id,
            ..Default::default()
        };

        // Leave all channels.
        for channel_id in connection_channels {
            self.leave_channel(connection_id, channel_id);
        }

        // Unregister and leave all projects.
        for project_id in connection_projects {
            if let Ok(project) = self.unregister_project(project_id, connection_id) {
                result.hosted_projects.insert(project_id, project);
            } else if self.leave_project(connection_id, project_id).is_ok() {
                result.guest_project_ids.insert(project_id);
            }
        }

        let user_connection_state = self.connections_by_user_id.get_mut(&user_id).unwrap();
        user_connection_state.connection_ids.remove(&connection_id);
        if user_connection_state.connection_ids.is_empty() {
            self.connections_by_user_id.remove(&user_id);
        }

        self.connections.remove(&connection_id).unwrap();

        Ok(result)
    }

    #[cfg(test)]
    pub fn channel(&self, id: ChannelId) -> Option<&Channel> {
        self.channels.get(&id)
    }

    pub fn join_channel(&mut self, connection_id: ConnectionId, channel_id: ChannelId) {
        if let Some(connection) = self.connections.get_mut(&connection_id) {
            connection.channels.insert(channel_id);
            self.channels
                .entry(channel_id)
                .or_default()
                .connection_ids
                .insert(connection_id);
        }
    }

    pub fn leave_channel(&mut self, connection_id: ConnectionId, channel_id: ChannelId) {
        if let Some(connection) = self.connections.get_mut(&connection_id) {
            connection.channels.remove(&channel_id);
            if let btree_map::Entry::Occupied(mut entry) = self.channels.entry(channel_id) {
                entry.get_mut().connection_ids.remove(&connection_id);
                if entry.get_mut().connection_ids.is_empty() {
                    entry.remove();
                }
            }
        }
    }

    pub fn user_id_for_connection(&self, connection_id: ConnectionId) -> Result<UserId> {
        Ok(self
            .connections
            .get(&connection_id)
            .ok_or_else(|| anyhow!("unknown connection"))?
            .user_id)
    }

    pub fn connection_ids_for_user(
        &self,
        user_id: UserId,
    ) -> impl Iterator<Item = ConnectionId> + '_ {
        self.connections_by_user_id
            .get(&user_id)
            .into_iter()
            .map(|state| &state.connection_ids)
            .flatten()
            .copied()
    }

    pub fn is_user_online(&self, user_id: UserId) -> bool {
        !self
            .connections_by_user_id
            .get(&user_id)
            .unwrap_or(&Default::default())
            .connection_ids
            .is_empty()
    }

    pub fn build_initial_contacts_update(
        &self,
        contacts: Vec<db::Contact>,
    ) -> proto::UpdateContacts {
        let mut update = proto::UpdateContacts::default();

        for contact in contacts {
            match contact {
                db::Contact::Accepted {
                    user_id,
                    should_notify,
                } => {
                    update
                        .contacts
                        .push(self.contact_for_user(user_id, should_notify));
                }
                db::Contact::Outgoing { user_id } => {
                    update.outgoing_requests.push(user_id.to_proto())
                }
                db::Contact::Incoming {
                    user_id,
                    should_notify,
                } => update
                    .incoming_requests
                    .push(proto::IncomingContactRequest {
                        requester_id: user_id.to_proto(),
                        should_notify,
                    }),
            }
        }

        update
    }

    pub fn contact_for_user(&self, user_id: UserId, should_notify: bool) -> proto::Contact {
        proto::Contact {
            user_id: user_id.to_proto(),
            projects: self.project_metadata_for_user(user_id),
            online: self.is_user_online(user_id),
            should_notify,
        }
    }

    pub fn project_metadata_for_user(&self, user_id: UserId) -> Vec<proto::ProjectMetadata> {
        let user_connection_state = self.connections_by_user_id.get(&user_id);
        let project_ids = user_connection_state.iter().flat_map(|state| {
            state
                .connection_ids
                .iter()
                .filter_map(|connection_id| self.connections.get(connection_id))
                .flat_map(|connection| connection.projects.iter().copied())
        });

        let mut metadata = Vec::new();
        for project_id in project_ids {
            if let Some(project) = self.projects.get(&project_id) {
                if project.host.user_id == user_id && project.online {
                    metadata.push(proto::ProjectMetadata {
                        id: project_id.to_proto(),
                        visible_worktree_root_names: project
                            .worktrees
                            .values()
                            .filter(|worktree| worktree.visible)
                            .map(|worktree| worktree.root_name.clone())
                            .collect(),
                        guests: project
                            .guests
                            .values()
                            .map(|guest| guest.user_id.to_proto())
                            .collect(),
                    });
                }
            }
        }

        metadata
    }

    pub fn create_room(&mut self, creator_connection_id: ConnectionId) -> Result<RoomId> {
        let connection = self
            .connections
            .get_mut(&creator_connection_id)
            .ok_or_else(|| anyhow!("no such connection"))?;
        let user_connection_state = self
            .connections_by_user_id
            .get_mut(&connection.user_id)
            .ok_or_else(|| anyhow!("no such connection"))?;
        anyhow::ensure!(
            user_connection_state.room.is_none(),
            "cannot participate in more than one room at once"
        );

        let mut room = proto::Room::default();
        room.participants.push(proto::Participant {
            user_id: connection.user_id.to_proto(),
            peer_id: creator_connection_id.0,
            project_ids: Default::default(),
            location: Some(proto::ParticipantLocation {
                variant: Some(proto::participant_location::Variant::External(
                    proto::participant_location::External {},
                )),
            }),
        });

        let room_id = post_inc(&mut self.next_room_id);
        self.rooms.insert(room_id, room);
        user_connection_state.room = Some(RoomState::Joined);
        Ok(room_id)
    }

    pub fn join_room(
        &mut self,
        room_id: u64,
        connection_id: ConnectionId,
    ) -> Result<(&proto::Room, Vec<ConnectionId>)> {
        let connection = self
            .connections
            .get_mut(&connection_id)
            .ok_or_else(|| anyhow!("no such connection"))?;
        let user_id = connection.user_id;
        let recipient_connection_ids = self.connection_ids_for_user(user_id).collect::<Vec<_>>();

        let mut user_connection_state = self
            .connections_by_user_id
            .get_mut(&user_id)
            .ok_or_else(|| anyhow!("no such connection"))?;
        anyhow::ensure!(
            user_connection_state
                .room
                .map_or(true, |room| room == RoomState::Calling { room_id }),
            "cannot participate in more than one room at once"
        );

        let room = self
            .rooms
            .get_mut(&room_id)
            .ok_or_else(|| anyhow!("no such room"))?;
        anyhow::ensure!(
            room.pending_user_ids.contains(&user_id.to_proto()),
            anyhow!("no such room")
        );
        room.pending_user_ids
            .retain(|pending| *pending != user_id.to_proto());
        room.participants.push(proto::Participant {
            user_id: user_id.to_proto(),
            peer_id: connection_id.0,
            project_ids: Default::default(),
            location: Some(proto::ParticipantLocation {
                variant: Some(proto::participant_location::Variant::External(
                    proto::participant_location::External {},
                )),
            }),
        });
        user_connection_state.room = Some(RoomState::Joined);

        Ok((room, recipient_connection_ids))
    }

    pub fn call(
        &mut self,
        room_id: RoomId,
        from_connection_id: ConnectionId,
        to_user_id: UserId,
    ) -> Result<(UserId, Vec<ConnectionId>, &proto::Room)> {
        let from_user_id = self.user_id_for_connection(from_connection_id)?;

        let to_connection_ids = self.connection_ids_for_user(to_user_id).collect::<Vec<_>>();
        let mut to_user_connection_state = self
            .connections_by_user_id
            .get_mut(&to_user_id)
            .ok_or_else(|| anyhow!("no such connection"))?;
        anyhow::ensure!(
            to_user_connection_state.room.is_none(),
            "recipient is already on another call"
        );

        let room = self
            .rooms
            .get_mut(&room_id)
            .ok_or_else(|| anyhow!("no such room"))?;
        anyhow::ensure!(
            room.participants
                .iter()
                .any(|participant| participant.peer_id == from_connection_id.0),
            "no such room"
        );
        anyhow::ensure!(
            room.pending_user_ids
                .iter()
                .all(|user_id| UserId::from_proto(*user_id) != to_user_id),
            "cannot call the same user more than once"
        );
        room.pending_user_ids.push(to_user_id.to_proto());
        to_user_connection_state.room = Some(RoomState::Calling { room_id });

        Ok((from_user_id, to_connection_ids, room))
    }

    pub fn call_failed(&mut self, room_id: RoomId, to_user_id: UserId) -> Result<&proto::Room> {
        let mut to_user_connection_state = self
            .connections_by_user_id
            .get_mut(&to_user_id)
            .ok_or_else(|| anyhow!("no such connection"))?;
        anyhow::ensure!(to_user_connection_state.room == Some(RoomState::Calling { room_id }));
        to_user_connection_state.room = None;
        let room = self
            .rooms
            .get_mut(&room_id)
            .ok_or_else(|| anyhow!("no such room"))?;
        room.pending_user_ids
            .retain(|user_id| UserId::from_proto(*user_id) != to_user_id);
        Ok(room)
    }

    pub fn call_declined(
        &mut self,
        recipient_connection_id: ConnectionId,
    ) -> Result<(&proto::Room, Vec<ConnectionId>)> {
        let recipient_user_id = self.user_id_for_connection(recipient_connection_id)?;
        let mut to_user_connection_state = self
            .connections_by_user_id
            .get_mut(&recipient_user_id)
            .ok_or_else(|| anyhow!("no such connection"))?;
        if let Some(RoomState::Calling { room_id }) = to_user_connection_state.room {
            to_user_connection_state.room = None;
            let recipient_connection_ids = self
                .connection_ids_for_user(recipient_user_id)
                .collect::<Vec<_>>();
            let room = self
                .rooms
                .get_mut(&room_id)
                .ok_or_else(|| anyhow!("no such room"))?;
            room.pending_user_ids
                .retain(|user_id| UserId::from_proto(*user_id) != recipient_user_id);
            Ok((room, recipient_connection_ids))
        } else {
            Err(anyhow!("user is not being called"))
        }
    }

    pub fn register_project(
        &mut self,
        host_connection_id: ConnectionId,
        project_id: ProjectId,
        online: bool,
    ) -> Result<()> {
        let connection = self
            .connections
            .get_mut(&host_connection_id)
            .ok_or_else(|| anyhow!("no such connection"))?;
        connection.projects.insert(project_id);
        self.projects.insert(
            project_id,
            Project {
                online,
                host_connection_id,
                host: Collaborator {
                    user_id: connection.user_id,
                    replica_id: 0,
                    last_activity: None,
                    admin: connection.admin,
                },
                guests: Default::default(),
                join_requests: Default::default(),
                active_replica_ids: Default::default(),
                worktrees: Default::default(),
                language_servers: Default::default(),
            },
        );
        Ok(())
    }

    pub fn update_project(
        &mut self,
        project_id: ProjectId,
        worktrees: &[proto::WorktreeMetadata],
        online: bool,
        connection_id: ConnectionId,
    ) -> Result<Option<UnsharedProject>> {
        let project = self
            .projects
            .get_mut(&project_id)
            .ok_or_else(|| anyhow!("no such project"))?;
        if project.host_connection_id == connection_id {
            let mut old_worktrees = mem::take(&mut project.worktrees);
            for worktree in worktrees {
                if let Some(old_worktree) = old_worktrees.remove(&worktree.id) {
                    project.worktrees.insert(worktree.id, old_worktree);
                } else {
                    project.worktrees.insert(
                        worktree.id,
                        Worktree {
                            root_name: worktree.root_name.clone(),
                            visible: worktree.visible,
                            ..Default::default()
                        },
                    );
                }
            }

            if online != project.online {
                project.online = online;
                if project.online {
                    Ok(None)
                } else {
                    for connection_id in project.guest_connection_ids() {
                        if let Some(connection) = self.connections.get_mut(&connection_id) {
                            connection.projects.remove(&project_id);
                        }
                    }

                    project.active_replica_ids.clear();
                    project.language_servers.clear();
                    for worktree in project.worktrees.values_mut() {
                        worktree.diagnostic_summaries.clear();
                        worktree.entries.clear();
                    }

                    Ok(Some(UnsharedProject {
                        guests: mem::take(&mut project.guests),
                        pending_join_requests: mem::take(&mut project.join_requests),
                    }))
                }
            } else {
                Ok(None)
            }
        } else {
            Err(anyhow!("no such project"))?
        }
    }

    pub fn unregister_project(
        &mut self,
        project_id: ProjectId,
        connection_id: ConnectionId,
    ) -> Result<Project> {
        match self.projects.entry(project_id) {
            btree_map::Entry::Occupied(e) => {
                if e.get().host_connection_id == connection_id {
                    let project = e.remove();

                    if let Some(host_connection) = self.connections.get_mut(&connection_id) {
                        host_connection.projects.remove(&project_id);
                    }

                    for guest_connection in project.guests.keys() {
                        if let Some(connection) = self.connections.get_mut(guest_connection) {
                            connection.projects.remove(&project_id);
                        }
                    }

                    for requester_user_id in project.join_requests.keys() {
                        if let Some(requester_user_connection_state) =
                            self.connections_by_user_id.get_mut(requester_user_id)
                        {
                            for requester_connection_id in
                                &requester_user_connection_state.connection_ids
                            {
                                if let Some(requester_connection) =
                                    self.connections.get_mut(requester_connection_id)
                                {
                                    requester_connection.requested_projects.remove(&project_id);
                                }
                            }
                        }
                    }

                    Ok(project)
                } else {
                    Err(anyhow!("no such project"))?
                }
            }
            btree_map::Entry::Vacant(_) => Err(anyhow!("no such project"))?,
        }
    }

    pub fn update_diagnostic_summary(
        &mut self,
        project_id: ProjectId,
        worktree_id: u64,
        connection_id: ConnectionId,
        summary: proto::DiagnosticSummary,
    ) -> Result<Vec<ConnectionId>> {
        let project = self
            .projects
            .get_mut(&project_id)
            .ok_or_else(|| anyhow!("no such project"))?;
        if project.host_connection_id == connection_id {
            let worktree = project
                .worktrees
                .get_mut(&worktree_id)
                .ok_or_else(|| anyhow!("no such worktree"))?;
            worktree
                .diagnostic_summaries
                .insert(summary.path.clone().into(), summary);
            return Ok(project.connection_ids());
        }

        Err(anyhow!("no such worktree"))?
    }

    pub fn start_language_server(
        &mut self,
        project_id: ProjectId,
        connection_id: ConnectionId,
        language_server: proto::LanguageServer,
    ) -> Result<Vec<ConnectionId>> {
        let project = self
            .projects
            .get_mut(&project_id)
            .ok_or_else(|| anyhow!("no such project"))?;
        if project.host_connection_id == connection_id {
            project.language_servers.push(language_server);
            return Ok(project.connection_ids());
        }

        Err(anyhow!("no such project"))?
    }

    pub fn request_join_project(
        &mut self,
        requester_id: UserId,
        project_id: ProjectId,
        receipt: Receipt<proto::JoinProject>,
    ) -> Result<()> {
        let connection = self
            .connections
            .get_mut(&receipt.sender_id)
            .ok_or_else(|| anyhow!("no such connection"))?;
        let project = self
            .projects
            .get_mut(&project_id)
            .ok_or_else(|| anyhow!("no such project"))?;
        if project.online {
            connection.requested_projects.insert(project_id);
            project
                .join_requests
                .entry(requester_id)
                .or_default()
                .push(receipt);
            Ok(())
        } else {
            Err(anyhow!("no such project"))
        }
    }

    pub fn deny_join_project_request(
        &mut self,
        responder_connection_id: ConnectionId,
        requester_id: UserId,
        project_id: ProjectId,
    ) -> Option<Vec<Receipt<proto::JoinProject>>> {
        let project = self.projects.get_mut(&project_id)?;
        if responder_connection_id != project.host_connection_id {
            return None;
        }

        let receipts = project.join_requests.remove(&requester_id)?;
        for receipt in &receipts {
            let requester_connection = self.connections.get_mut(&receipt.sender_id)?;
            requester_connection.requested_projects.remove(&project_id);
        }
        project.host.last_activity = Some(OffsetDateTime::now_utc());

        Some(receipts)
    }

    #[allow(clippy::type_complexity)]
    pub fn accept_join_project_request(
        &mut self,
        responder_connection_id: ConnectionId,
        requester_id: UserId,
        project_id: ProjectId,
    ) -> Option<(Vec<(Receipt<proto::JoinProject>, ReplicaId)>, &Project)> {
        let project = self.projects.get_mut(&project_id)?;
        if responder_connection_id != project.host_connection_id {
            return None;
        }

        let receipts = project.join_requests.remove(&requester_id)?;
        let mut receipts_with_replica_ids = Vec::new();
        for receipt in receipts {
            let requester_connection = self.connections.get_mut(&receipt.sender_id)?;
            requester_connection.requested_projects.remove(&project_id);
            requester_connection.projects.insert(project_id);
            let mut replica_id = 1;
            while project.active_replica_ids.contains(&replica_id) {
                replica_id += 1;
            }
            project.active_replica_ids.insert(replica_id);
            project.guests.insert(
                receipt.sender_id,
                Collaborator {
                    replica_id,
                    user_id: requester_id,
                    last_activity: Some(OffsetDateTime::now_utc()),
                    admin: requester_connection.admin,
                },
            );
            receipts_with_replica_ids.push((receipt, replica_id));
        }

        project.host.last_activity = Some(OffsetDateTime::now_utc());
        Some((receipts_with_replica_ids, project))
    }

    pub fn leave_project(
        &mut self,
        connection_id: ConnectionId,
        project_id: ProjectId,
    ) -> Result<LeftProject> {
        let user_id = self.user_id_for_connection(connection_id)?;
        let project = self
            .projects
            .get_mut(&project_id)
            .ok_or_else(|| anyhow!("no such project"))?;

        // If the connection leaving the project is a collaborator, remove it.
        let remove_collaborator = if let Some(guest) = project.guests.remove(&connection_id) {
            project.active_replica_ids.remove(&guest.replica_id);
            true
        } else {
            false
        };

        // If the connection leaving the project has a pending request, remove it.
        // If that user has no other pending requests on other connections, indicate that the request should be cancelled.
        let mut cancel_request = None;
        if let Entry::Occupied(mut entry) = project.join_requests.entry(user_id) {
            entry
                .get_mut()
                .retain(|receipt| receipt.sender_id != connection_id);
            if entry.get().is_empty() {
                entry.remove();
                cancel_request = Some(user_id);
            }
        }

        if let Some(connection) = self.connections.get_mut(&connection_id) {
            connection.projects.remove(&project_id);
        }

        let connection_ids = project.connection_ids();
        let unshare = connection_ids.len() <= 1 && project.join_requests.is_empty();
        if unshare {
            project.language_servers.clear();
            for worktree in project.worktrees.values_mut() {
                worktree.diagnostic_summaries.clear();
                worktree.entries.clear();
            }
        }

        Ok(LeftProject {
            host_connection_id: project.host_connection_id,
            host_user_id: project.host.user_id,
            connection_ids,
            cancel_request,
            unshare,
            remove_collaborator,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_worktree(
        &mut self,
        connection_id: ConnectionId,
        project_id: ProjectId,
        worktree_id: u64,
        worktree_root_name: &str,
        removed_entries: &[u64],
        updated_entries: &[proto::Entry],
        scan_id: u64,
        is_last_update: bool,
    ) -> Result<(Vec<ConnectionId>, bool)> {
        let project = self.write_project(project_id, connection_id)?;
        if !project.online {
            return Err(anyhow!("project is not online"));
        }

        let connection_ids = project.connection_ids();
        let mut worktree = project.worktrees.entry(worktree_id).or_default();
        let metadata_changed = worktree_root_name != worktree.root_name;
        worktree.root_name = worktree_root_name.to_string();

        for entry_id in removed_entries {
            worktree.entries.remove(entry_id);
        }

        for entry in updated_entries {
            worktree.entries.insert(entry.id, entry.clone());
        }

        worktree.scan_id = scan_id;
        worktree.is_complete = is_last_update;
        Ok((connection_ids, metadata_changed))
    }

    pub fn project_connection_ids(
        &self,
        project_id: ProjectId,
        acting_connection_id: ConnectionId,
    ) -> Result<Vec<ConnectionId>> {
        Ok(self
            .read_project(project_id, acting_connection_id)?
            .connection_ids())
    }

    pub fn channel_connection_ids(&self, channel_id: ChannelId) -> Result<Vec<ConnectionId>> {
        Ok(self
            .channels
            .get(&channel_id)
            .ok_or_else(|| anyhow!("no such channel"))?
            .connection_ids())
    }

    pub fn project(&self, project_id: ProjectId) -> Result<&Project> {
        self.projects
            .get(&project_id)
            .ok_or_else(|| anyhow!("no such project"))
    }

    pub fn register_project_activity(
        &mut self,
        project_id: ProjectId,
        connection_id: ConnectionId,
    ) -> Result<()> {
        let project = self
            .projects
            .get_mut(&project_id)
            .ok_or_else(|| anyhow!("no such project"))?;
        let collaborator = if connection_id == project.host_connection_id {
            &mut project.host
        } else if let Some(guest) = project.guests.get_mut(&connection_id) {
            guest
        } else {
            return Err(anyhow!("no such project"))?;
        };
        collaborator.last_activity = Some(OffsetDateTime::now_utc());
        Ok(())
    }

    pub fn projects(&self) -> impl Iterator<Item = (&ProjectId, &Project)> {
        self.projects.iter()
    }

    pub fn read_project(
        &self,
        project_id: ProjectId,
        connection_id: ConnectionId,
    ) -> Result<&Project> {
        let project = self
            .projects
            .get(&project_id)
            .ok_or_else(|| anyhow!("no such project"))?;
        if project.host_connection_id == connection_id
            || project.guests.contains_key(&connection_id)
        {
            Ok(project)
        } else {
            Err(anyhow!("no such project"))?
        }
    }

    fn write_project(
        &mut self,
        project_id: ProjectId,
        connection_id: ConnectionId,
    ) -> Result<&mut Project> {
        let project = self
            .projects
            .get_mut(&project_id)
            .ok_or_else(|| anyhow!("no such project"))?;
        if project.host_connection_id == connection_id
            || project.guests.contains_key(&connection_id)
        {
            Ok(project)
        } else {
            Err(anyhow!("no such project"))?
        }
    }

    #[cfg(test)]
    pub fn check_invariants(&self) {
        for (connection_id, connection) in &self.connections {
            for project_id in &connection.projects {
                let project = &self.projects.get(project_id).unwrap();
                if project.host_connection_id != *connection_id {
                    assert!(project.guests.contains_key(connection_id));
                }

                for (worktree_id, worktree) in project.worktrees.iter() {
                    let mut paths = HashMap::default();
                    for entry in worktree.entries.values() {
                        let prev_entry = paths.insert(&entry.path, entry);
                        assert_eq!(
                            prev_entry,
                            None,
                            "worktree {:?}, duplicate path for entries {:?} and {:?}",
                            worktree_id,
                            prev_entry.unwrap(),
                            entry
                        );
                    }
                }
            }
            for channel_id in &connection.channels {
                let channel = self.channels.get(channel_id).unwrap();
                assert!(channel.connection_ids.contains(connection_id));
            }
            assert!(self
                .connections_by_user_id
                .get(&connection.user_id)
                .unwrap()
                .connection_ids
                .contains(connection_id));
        }

        for (user_id, state) in &self.connections_by_user_id {
            for connection_id in &state.connection_ids {
                assert_eq!(
                    self.connections.get(connection_id).unwrap().user_id,
                    *user_id
                );
            }
        }

        for (project_id, project) in &self.projects {
            let host_connection = self.connections.get(&project.host_connection_id).unwrap();
            assert!(host_connection.projects.contains(project_id));

            for guest_connection_id in project.guests.keys() {
                let guest_connection = self.connections.get(guest_connection_id).unwrap();
                assert!(guest_connection.projects.contains(project_id));
            }
            assert_eq!(project.active_replica_ids.len(), project.guests.len(),);
            assert_eq!(
                project.active_replica_ids,
                project
                    .guests
                    .values()
                    .map(|guest| guest.replica_id)
                    .collect::<HashSet<_>>(),
            );
        }

        for (channel_id, channel) in &self.channels {
            for connection_id in &channel.connection_ids {
                let connection = self.connections.get(connection_id).unwrap();
                assert!(connection.channels.contains(channel_id));
            }
        }
    }
}

impl Project {
    fn is_active_since(&self, start_time: OffsetDateTime) -> bool {
        self.guests
            .values()
            .chain([&self.host])
            .any(|collaborator| {
                collaborator
                    .last_activity
                    .map_or(false, |active_time| active_time > start_time)
            })
    }

    pub fn guest_connection_ids(&self) -> Vec<ConnectionId> {
        self.guests.keys().copied().collect()
    }

    pub fn connection_ids(&self) -> Vec<ConnectionId> {
        self.guests
            .keys()
            .copied()
            .chain(Some(self.host_connection_id))
            .collect()
    }
}

impl Channel {
    fn connection_ids(&self) -> Vec<ConnectionId> {
        self.connection_ids.iter().copied().collect()
    }
}
