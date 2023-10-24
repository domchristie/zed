use super::*;
use rpc::proto::{channel_member::Kind, ChannelEdge};

impl Database {
    #[cfg(test)]
    pub async fn all_channels(&self) -> Result<Vec<(ChannelId, String)>> {
        self.transaction(move |tx| async move {
            let mut channels = Vec::new();
            let mut rows = channel::Entity::find().stream(&*tx).await?;
            while let Some(row) = rows.next().await {
                let row = row?;
                channels.push((row.id, row.name));
            }
            Ok(channels)
        })
        .await
    }

    #[cfg(test)]
    pub async fn create_root_channel(&self, name: &str, creator_id: UserId) -> Result<ChannelId> {
        Ok(self
            .create_channel(name, None, creator_id)
            .await?
            .channel
            .id)
    }

    #[cfg(test)]
    pub async fn create_sub_channel(
        &self,
        name: &str,
        parent: ChannelId,
        creator_id: UserId,
    ) -> Result<ChannelId> {
        Ok(self
            .create_channel(name, Some(parent), creator_id)
            .await?
            .channel
            .id)
    }

    pub async fn create_channel(
        &self,
        name: &str,
        parent: Option<ChannelId>,
        admin_id: UserId,
    ) -> Result<CreateChannelResult> {
        let name = Self::sanitize_channel_name(name)?;
        self.transaction(move |tx| async move {
            if let Some(parent) = parent {
                self.check_user_is_channel_admin(parent, admin_id, &*tx)
                    .await?;
            }

            let channel = channel::ActiveModel {
                id: ActiveValue::NotSet,
                name: ActiveValue::Set(name.to_string()),
                visibility: ActiveValue::Set(ChannelVisibility::Members),
            }
            .insert(&*tx)
            .await?;

            if let Some(parent) = parent {
                let sql = r#"
                    INSERT INTO channel_paths
                    (id_path, channel_id)
                    SELECT
                        id_path || $1 || '/', $2
                    FROM
                        channel_paths
                    WHERE
                        channel_id = $3
                "#;
                let channel_paths_stmt = Statement::from_sql_and_values(
                    self.pool.get_database_backend(),
                    sql,
                    [
                        channel.id.to_proto().into(),
                        channel.id.to_proto().into(),
                        parent.to_proto().into(),
                    ],
                );
                tx.execute(channel_paths_stmt).await?;
            } else {
                channel_path::Entity::insert(channel_path::ActiveModel {
                    channel_id: ActiveValue::Set(channel.id),
                    id_path: ActiveValue::Set(format!("/{}/", channel.id)),
                })
                .exec(&*tx)
                .await?;
            }

            if parent.is_none() {
                channel_member::ActiveModel {
                    id: ActiveValue::NotSet,
                    channel_id: ActiveValue::Set(channel.id),
                    user_id: ActiveValue::Set(admin_id),
                    accepted: ActiveValue::Set(true),
                    role: ActiveValue::Set(ChannelRole::Admin),
                }
                .insert(&*tx)
                .await?;
            }

            let participants_to_update = if let Some(parent) = parent {
                self.participants_to_notify_for_channel_change(parent, &*tx)
                    .await?
            } else {
                vec![]
            };

            Ok(CreateChannelResult {
                channel: Channel {
                    id: channel.id,
                    visibility: channel.visibility,
                    name: channel.name,
                    role: ChannelRole::Admin,
                },
                participants_to_update,
            })
        })
        .await
    }

    pub async fn join_channel(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        connection: ConnectionId,
        environment: &str,
    ) -> Result<(JoinRoom, Option<MembershipUpdated>, ChannelRole)> {
        self.transaction(move |tx| async move {
            let mut accept_invite_result = None;

            let channel = channel::Entity::find()
                .filter(channel::Column::Id.eq(channel_id))
                .one(&*tx)
                .await?;

            let mut role = self
                .channel_role_for_user(channel_id, user_id, &*tx)
                .await?;

            if role.is_none() && channel.is_some() {
                if let Some(invitation) = self
                    .pending_invite_for_channel(channel_id, user_id, &*tx)
                    .await?
                {
                    // note, this may be a parent channel
                    role = Some(invitation.role);
                    channel_member::Entity::update(channel_member::ActiveModel {
                        accepted: ActiveValue::Set(true),
                        ..invitation.into_active_model()
                    })
                    .exec(&*tx)
                    .await?;

                    accept_invite_result = Some(
                        self.calculate_membership_updated(channel_id, user_id, &*tx)
                            .await?,
                    );

                    debug_assert!(
                        self.channel_role_for_user(channel_id, user_id, &*tx)
                            .await?
                            == role
                    );
                }
            }
            if role.is_none()
                && channel.as_ref().map(|c| c.visibility) == Some(ChannelVisibility::Public)
            {
                role = Some(ChannelRole::Guest);
                let channel_id_to_join = self
                    .public_path_to_channel(channel_id, &*tx)
                    .await?
                    .first()
                    .cloned()
                    .unwrap_or(channel_id);

                channel_member::Entity::insert(channel_member::ActiveModel {
                    id: ActiveValue::NotSet,
                    channel_id: ActiveValue::Set(channel_id_to_join),
                    user_id: ActiveValue::Set(user_id),
                    accepted: ActiveValue::Set(true),
                    role: ActiveValue::Set(ChannelRole::Guest),
                })
                .exec(&*tx)
                .await?;

                accept_invite_result = Some(
                    self.calculate_membership_updated(channel_id, user_id, &*tx)
                        .await?,
                );

                debug_assert!(
                    self.channel_role_for_user(channel_id, user_id, &*tx)
                        .await?
                        == role
                );
            }

            if channel.is_none() || role.is_none() || role == Some(ChannelRole::Banned) {
                Err(anyhow!("no such channel, or not allowed"))?
            }

            let live_kit_room = format!("channel-{}", nanoid::nanoid!(30));
            let room_id = self
                .get_or_create_channel_room(channel_id, &live_kit_room, environment, &*tx)
                .await?;

            self.join_channel_room_internal(channel_id, room_id, user_id, connection, &*tx)
                .await
                .map(|jr| (jr, accept_invite_result, role.unwrap()))
        })
        .await
    }

    pub async fn set_channel_visibility(
        &self,
        channel_id: ChannelId,
        visibility: ChannelVisibility,
        admin_id: UserId,
    ) -> Result<SetChannelVisibilityResult> {
        self.transaction(move |tx| async move {
            self.check_user_is_channel_admin(channel_id, admin_id, &*tx)
                .await?;

            let previous_members = self
                .get_channel_participant_details_internal(channel_id, &*tx)
                .await?;

            channel::ActiveModel {
                id: ActiveValue::Unchanged(channel_id),
                visibility: ActiveValue::Set(visibility),
                ..Default::default()
            }
            .update(&*tx)
            .await?;

            let mut participants_to_update: HashMap<UserId, ChannelsForUser> = self
                .participants_to_notify_for_channel_change(channel_id, &*tx)
                .await?
                .into_iter()
                .collect();

            let mut channels_to_remove: Vec<ChannelId> = vec![];
            let mut participants_to_remove: HashSet<UserId> = HashSet::default();
            match visibility {
                ChannelVisibility::Members => {
                    let all_descendents: Vec<ChannelId> = self
                        .get_channel_descendants(vec![channel_id], &*tx)
                        .await?
                        .into_iter()
                        .map(|edge| ChannelId::from_proto(edge.channel_id))
                        .collect();

                    channels_to_remove = channel::Entity::find()
                        .filter(
                            channel::Column::Id
                                .is_in(all_descendents)
                                .and(channel::Column::Visibility.eq(ChannelVisibility::Public)),
                        )
                        .all(&*tx)
                        .await?
                        .into_iter()
                        .map(|channel| channel.id)
                        .collect();

                    channels_to_remove.push(channel_id);
                    for member in previous_members {
                        if member.role.can_only_see_public_descendants() {
                            participants_to_remove.insert(member.user_id);
                        }
                    }
                }
                ChannelVisibility::Public => {
                    if let Some(public_parent_id) =
                        self.public_parent_channel_id(channel_id, &*tx).await?
                    {
                        let parent_updates = self
                            .participants_to_notify_for_channel_change(public_parent_id, &*tx)
                            .await?;

                        for (user_id, channels) in parent_updates {
                            participants_to_update.insert(user_id, channels);
                        }
                    }
                }
            }

            Ok(SetChannelVisibilityResult {
                participants_to_update,
                participants_to_remove,
                channels_to_remove,
            })
        })
        .await
    }

    pub async fn delete_channel(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
    ) -> Result<(Vec<ChannelId>, Vec<UserId>)> {
        self.transaction(move |tx| async move {
            self.check_user_is_channel_admin(channel_id, user_id, &*tx)
                .await?;

            // Don't remove descendant channels that have additional parents.
            let mut channels_to_remove: HashSet<ChannelId> = HashSet::default();
            channels_to_remove.insert(channel_id);

            let graph = self.get_channel_descendants([channel_id], &*tx).await?;
            for edge in graph.iter() {
                channels_to_remove.insert(ChannelId::from_proto(edge.channel_id));
            }

            {
                let mut channels_to_keep = channel_path::Entity::find()
                    .filter(
                        channel_path::Column::ChannelId
                            .is_in(channels_to_remove.iter().copied())
                            .and(
                                channel_path::Column::IdPath
                                    .not_like(&format!("%/{}/%", channel_id)),
                            ),
                    )
                    .stream(&*tx)
                    .await?;
                while let Some(row) = channels_to_keep.next().await {
                    let row = row?;
                    channels_to_remove.remove(&row.channel_id);
                }
            }

            let channel_ancestors = self.get_channel_ancestors(channel_id, &*tx).await?;
            let members_to_notify: Vec<UserId> = channel_member::Entity::find()
                .filter(channel_member::Column::ChannelId.is_in(channel_ancestors))
                .select_only()
                .column(channel_member::Column::UserId)
                .distinct()
                .into_values::<_, QueryUserIds>()
                .all(&*tx)
                .await?;

            channel::Entity::delete_many()
                .filter(channel::Column::Id.is_in(channels_to_remove.iter().copied()))
                .exec(&*tx)
                .await?;

            // Delete any other paths that include this channel
            let sql = r#"
                    DELETE FROM channel_paths
                    WHERE
                        id_path LIKE '%' || $1 || '%'
                "#;
            let channel_paths_stmt = Statement::from_sql_and_values(
                self.pool.get_database_backend(),
                sql,
                [channel_id.to_proto().into()],
            );
            tx.execute(channel_paths_stmt).await?;

            Ok((channels_to_remove.into_iter().collect(), members_to_notify))
        })
        .await
    }

    pub async fn invite_channel_member(
        &self,
        channel_id: ChannelId,
        invitee_id: UserId,
        inviter_id: UserId,
        role: ChannelRole,
    ) -> Result<InviteMemberResult> {
        self.transaction(move |tx| async move {
            self.check_user_is_channel_admin(channel_id, inviter_id, &*tx)
                .await?;

            channel_member::ActiveModel {
                id: ActiveValue::NotSet,
                channel_id: ActiveValue::Set(channel_id),
                user_id: ActiveValue::Set(invitee_id),
                accepted: ActiveValue::Set(false),
                role: ActiveValue::Set(role),
            }
            .insert(&*tx)
            .await?;

            let channel = channel::Entity::find_by_id(channel_id)
                .one(&*tx)
                .await?
                .unwrap();

            let channel = Channel {
                id: channel.id,
                visibility: channel.visibility,
                name: channel.name,
                role,
            };

            let notifications = self
                .create_notification(
                    invitee_id,
                    rpc::Notification::ChannelInvitation {
                        channel_id: channel_id.to_proto(),
                        channel_name: channel.name.clone(),
                        inviter_id: inviter_id.to_proto(),
                    },
                    true,
                    &*tx,
                )
                .await?
                .into_iter()
                .collect();

            Ok(InviteMemberResult {
                channel,
                notifications,
            })
        })
        .await
    }

    fn sanitize_channel_name(name: &str) -> Result<&str> {
        let new_name = name.trim().trim_start_matches('#');
        if new_name == "" {
            Err(anyhow!("channel name can't be blank"))?;
        }
        Ok(new_name)
    }

    pub async fn rename_channel(
        &self,
        channel_id: ChannelId,
        admin_id: UserId,
        new_name: &str,
    ) -> Result<RenameChannelResult> {
        self.transaction(move |tx| async move {
            let new_name = Self::sanitize_channel_name(new_name)?.to_string();

            let role = self
                .check_user_is_channel_admin(channel_id, admin_id, &*tx)
                .await?;

            let channel = channel::ActiveModel {
                id: ActiveValue::Unchanged(channel_id),
                name: ActiveValue::Set(new_name.clone()),
                ..Default::default()
            }
            .update(&*tx)
            .await?;

            let participants = self
                .get_channel_participant_details_internal(channel_id, &*tx)
                .await?;

            Ok(RenameChannelResult {
                channel: Channel {
                    id: channel.id,
                    name: channel.name,
                    visibility: channel.visibility,
                    role,
                },
                participants_to_update: participants
                    .iter()
                    .map(|participant| {
                        (
                            participant.user_id,
                            Channel {
                                id: channel.id,
                                name: new_name.clone(),
                                visibility: channel.visibility,
                                role: participant.role,
                            },
                        )
                    })
                    .collect(),
            })
        })
        .await
    }

    pub async fn respond_to_channel_invite(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        accept: bool,
    ) -> Result<RespondToChannelInvite> {
        self.transaction(move |tx| async move {
            let membership_update = if accept {
                let rows_affected = channel_member::Entity::update_many()
                    .set(channel_member::ActiveModel {
                        accepted: ActiveValue::Set(accept),
                        ..Default::default()
                    })
                    .filter(
                        channel_member::Column::ChannelId
                            .eq(channel_id)
                            .and(channel_member::Column::UserId.eq(user_id))
                            .and(channel_member::Column::Accepted.eq(false)),
                    )
                    .exec(&*tx)
                    .await?
                    .rows_affected;

                if rows_affected == 0 {
                    Err(anyhow!("no such invitation"))?;
                }

                Some(
                    self.calculate_membership_updated(channel_id, user_id, &*tx)
                        .await?,
                )
            } else {
                let rows_affected = channel_member::Entity::delete_many()
                    .filter(
                        channel_member::Column::ChannelId
                            .eq(channel_id)
                            .and(channel_member::Column::UserId.eq(user_id))
                            .and(channel_member::Column::Accepted.eq(false)),
                    )
                    .exec(&*tx)
                    .await?
                    .rows_affected;
                if rows_affected == 0 {
                    Err(anyhow!("no such invitation"))?;
                }

                None
            };

            Ok(RespondToChannelInvite {
                membership_update,
                notifications: self
                    .mark_notification_as_read_with_response(
                        user_id,
                        &rpc::Notification::ChannelInvitation {
                            channel_id: channel_id.to_proto(),
                            channel_name: Default::default(),
                            inviter_id: Default::default(),
                        },
                        accept,
                        &*tx,
                    )
                    .await?
                    .into_iter()
                    .collect(),
            })
        })
        .await
    }

    async fn calculate_membership_updated(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        tx: &DatabaseTransaction,
    ) -> Result<MembershipUpdated> {
        let mut channel_to_refresh = channel_id;
        let mut removed_channels: Vec<ChannelId> = Vec::new();

        // if the user was previously a guest of a parent public channel they may have seen this
        // channel (or its descendants) in the tree already.
        // Now they have new permissions, the graph of channels needs updating from that point.
        if let Some(public_parent) = self.public_parent_channel_id(channel_id, &*tx).await? {
            if self
                .channel_role_for_user(public_parent, user_id, &*tx)
                .await?
                == Some(ChannelRole::Guest)
            {
                channel_to_refresh = public_parent;
            }
        }

        // remove all descendant channels from the user's tree
        removed_channels.append(
            &mut self
                .get_channel_descendants(vec![channel_to_refresh], &*tx)
                .await?
                .into_iter()
                .map(|edge| ChannelId::from_proto(edge.channel_id))
                .collect(),
        );

        let new_channels = self
            .get_user_channels(user_id, Some(channel_to_refresh), &*tx)
            .await?;

        // We only add the current channel to "moved" if the user has lost access,
        // otherwise it would be made a root channel on the client.
        if !new_channels
            .channels
            .channels
            .iter()
            .any(|c| c.id == channel_id)
        {
            removed_channels.push(channel_id);
        }

        Ok(MembershipUpdated {
            channel_id,
            new_channels,
            removed_channels,
        })
    }

    pub async fn remove_channel_member(
        &self,
        channel_id: ChannelId,
        member_id: UserId,
        admin_id: UserId,
    ) -> Result<RemoveChannelMemberResult> {
        self.transaction(|tx| async move {
            self.check_user_is_channel_admin(channel_id, admin_id, &*tx)
                .await?;

            let result = channel_member::Entity::delete_many()
                .filter(
                    channel_member::Column::ChannelId
                        .eq(channel_id)
                        .and(channel_member::Column::UserId.eq(member_id)),
                )
                .exec(&*tx)
                .await?;

            if result.rows_affected == 0 {
                Err(anyhow!("no such member"))?;
            }

            Ok(RemoveChannelMemberResult {
                membership_update: self
                    .calculate_membership_updated(channel_id, member_id, &*tx)
                    .await?,
                notification_id: self
                    .remove_notification(
                        member_id,
                        rpc::Notification::ChannelInvitation {
                            channel_id: channel_id.to_proto(),
                            channel_name: Default::default(),
                            inviter_id: Default::default(),
                        },
                        &*tx,
                    )
                    .await?,
            })
        })
        .await
    }

    pub async fn get_channel_invites_for_user(&self, user_id: UserId) -> Result<Vec<Channel>> {
        self.transaction(|tx| async move {
            let mut role_for_channel: HashMap<ChannelId, ChannelRole> = HashMap::default();

            let channel_invites = channel_member::Entity::find()
                .filter(
                    channel_member::Column::UserId
                        .eq(user_id)
                        .and(channel_member::Column::Accepted.eq(false)),
                )
                .all(&*tx)
                .await?;

            for invite in channel_invites {
                role_for_channel.insert(invite.channel_id, invite.role);
            }

            let channels = channel::Entity::find()
                .filter(channel::Column::Id.is_in(role_for_channel.keys().copied()))
                .all(&*tx)
                .await?;

            let channels = channels
                .into_iter()
                .map(|channel| Channel {
                    id: channel.id,
                    name: channel.name,
                    visibility: channel.visibility,
                    role: role_for_channel[&channel.id],
                })
                .collect();

            Ok(channels)
        })
        .await
    }

    pub async fn get_channels_for_user(&self, user_id: UserId) -> Result<ChannelsForUser> {
        self.transaction(|tx| async move {
            let tx = tx;

            self.get_user_channels(user_id, None, &tx).await
        })
        .await
    }

    pub async fn get_user_channels(
        &self,
        user_id: UserId,
        parent_channel_id: Option<ChannelId>,
        tx: &DatabaseTransaction,
    ) -> Result<ChannelsForUser> {
        // note: we could (maybe) win some efficiency here when parent_channel_id
        // is set by getting just the role for that channel, then getting descendants
        // with roles attached; but that's not as straightforward as it sounds
        // because we need to calculate the path to the channel to make the query
        // efficient, which currently requires an extra round trip to the database.
        // Fix this later...
        let channel_memberships = channel_member::Entity::find()
            .filter(
                channel_member::Column::UserId
                    .eq(user_id)
                    .and(channel_member::Column::Accepted.eq(true)),
            )
            .all(&*tx)
            .await?;

        let mut edges = self
            .get_channel_descendants(channel_memberships.iter().map(|m| m.channel_id), &*tx)
            .await?;

        let mut role_for_channel: HashMap<ChannelId, (ChannelRole, bool)> = HashMap::default();

        for membership in channel_memberships.iter() {
            let included =
                parent_channel_id.is_none() || membership.channel_id == parent_channel_id.unwrap();
            role_for_channel.insert(membership.channel_id, (membership.role, included));
        }

        for ChannelEdge {
            parent_id,
            channel_id,
        } in edges.iter()
        {
            let parent_id = ChannelId::from_proto(*parent_id);
            let channel_id = ChannelId::from_proto(*channel_id);
            debug_assert!(role_for_channel.get(&parent_id).is_some());
            let (parent_role, parent_included) = role_for_channel[&parent_id];

            if let Some((existing_role, included)) = role_for_channel.get(&channel_id) {
                role_for_channel.insert(
                    channel_id,
                    (existing_role.max(parent_role), *included || parent_included),
                );
            } else {
                role_for_channel.insert(
                    channel_id,
                    (
                        parent_role,
                        parent_included
                            || parent_channel_id.is_none()
                            || Some(channel_id) == parent_channel_id,
                    ),
                );
            }
        }

        let mut channels: Vec<Channel> = Vec::new();
        let mut channels_to_remove: HashSet<u64> = HashSet::default();

        let mut rows = channel::Entity::find()
            .filter(channel::Column::Id.is_in(role_for_channel.keys().copied()))
            .stream(&*tx)
            .await?;

        while let Some(row) = rows.next().await {
            let channel = row?;
            let (role, included) = role_for_channel[&channel.id];

            if !included
                || role == ChannelRole::Banned
                || role == ChannelRole::Guest && channel.visibility != ChannelVisibility::Public
            {
                channels_to_remove.insert(channel.id.0 as u64);
                continue;
            }

            channels.push(Channel {
                id: channel.id,
                name: channel.name,
                visibility: channel.visibility,
                role,
            });
        }
        drop(rows);

        if !channels_to_remove.is_empty() {
            // Note: this code assumes each channel has one parent.
            // If there are multiple valid public paths to a channel,
            // e.g.
            // If both of these paths are present (* indicating public):
            // - zed* -> projects -> vim*
            // - zed* -> conrad -> public-projects* -> vim*
            // Users would only see one of them (based on edge sort order)
            let mut replacement_parent: HashMap<u64, u64> = HashMap::default();
            for ChannelEdge {
                parent_id,
                channel_id,
            } in edges.iter()
            {
                if channels_to_remove.contains(channel_id) {
                    replacement_parent.insert(*channel_id, *parent_id);
                }
            }

            let mut new_edges: Vec<ChannelEdge> = Vec::new();
            'outer: for ChannelEdge {
                mut parent_id,
                channel_id,
            } in edges.iter()
            {
                if channels_to_remove.contains(channel_id) {
                    continue;
                }
                while channels_to_remove.contains(&parent_id) {
                    if let Some(new_parent_id) = replacement_parent.get(&parent_id) {
                        parent_id = *new_parent_id;
                    } else {
                        continue 'outer;
                    }
                }
                new_edges.push(ChannelEdge {
                    parent_id,
                    channel_id: *channel_id,
                })
            }
            edges = new_edges;
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveColumn)]
        enum QueryUserIdsAndChannelIds {
            ChannelId,
            UserId,
        }

        let mut channel_participants: HashMap<ChannelId, Vec<UserId>> = HashMap::default();
        {
            let mut rows = room_participant::Entity::find()
                .inner_join(room::Entity)
                .filter(room::Column::ChannelId.is_in(channels.iter().map(|c| c.id)))
                .select_only()
                .column(room::Column::ChannelId)
                .column(room_participant::Column::UserId)
                .into_values::<_, QueryUserIdsAndChannelIds>()
                .stream(&*tx)
                .await?;
            while let Some(row) = rows.next().await {
                let row: (ChannelId, UserId) = row?;
                channel_participants.entry(row.0).or_default().push(row.1)
            }
        }

        let channel_ids = channels.iter().map(|c| c.id).collect::<Vec<_>>();
        let channel_buffer_changes = self
            .unseen_channel_buffer_changes(user_id, &channel_ids, &*tx)
            .await?;

        let unseen_messages = self
            .unseen_channel_messages(user_id, &channel_ids, &*tx)
            .await?;

        Ok(ChannelsForUser {
            channels: ChannelGraph { channels, edges },
            channel_participants,
            unseen_buffer_changes: channel_buffer_changes,
            channel_messages: unseen_messages,
        })
    }

    async fn participants_to_notify_for_channel_change(
        &self,
        new_parent: ChannelId,
        tx: &DatabaseTransaction,
    ) -> Result<Vec<(UserId, ChannelsForUser)>> {
        let mut results: Vec<(UserId, ChannelsForUser)> = Vec::new();

        let members = self
            .get_channel_participant_details_internal(new_parent, &*tx)
            .await?;

        for member in members.iter() {
            if !member.role.can_see_all_descendants() {
                continue;
            }
            results.push((
                member.user_id,
                self.get_user_channels(member.user_id, Some(new_parent), &*tx)
                    .await?,
            ))
        }

        let public_parent = self
            .public_path_to_channel(new_parent, &*tx)
            .await?
            .last()
            .copied();

        let Some(public_parent) = public_parent else {
            return Ok(results);
        };

        // could save some time in the common case by skipping this if the
        // new channel is not public and has no public descendants.
        let public_members = if public_parent == new_parent {
            members
        } else {
            self.get_channel_participant_details_internal(public_parent, &*tx)
                .await?
        };

        for member in public_members {
            if !member.role.can_only_see_public_descendants() {
                continue;
            };
            results.push((
                member.user_id,
                self.get_user_channels(member.user_id, Some(public_parent), &*tx)
                    .await?,
            ))
        }

        Ok(results)
    }

    pub async fn set_channel_member_role(
        &self,
        channel_id: ChannelId,
        admin_id: UserId,
        for_user: UserId,
        role: ChannelRole,
    ) -> Result<SetMemberRoleResult> {
        self.transaction(|tx| async move {
            self.check_user_is_channel_admin(channel_id, admin_id, &*tx)
                .await?;

            let membership = channel_member::Entity::find()
                .filter(
                    channel_member::Column::ChannelId
                        .eq(channel_id)
                        .and(channel_member::Column::UserId.eq(for_user)),
                )
                .one(&*tx)
                .await?;

            let Some(membership) = membership else {
                Err(anyhow!("no such member"))?
            };

            let mut update = membership.into_active_model();
            update.role = ActiveValue::Set(role);
            let updated = channel_member::Entity::update(update).exec(&*tx).await?;

            if !updated.accepted {
                let channel = channel::Entity::find_by_id(channel_id)
                    .one(&*tx)
                    .await?
                    .unwrap();

                return Ok(SetMemberRoleResult::InviteUpdated(Channel {
                    id: channel.id,
                    visibility: channel.visibility,
                    name: channel.name,
                    role,
                }));
            }

            Ok(SetMemberRoleResult::MembershipUpdated(
                self.calculate_membership_updated(channel_id, for_user, &*tx)
                    .await?,
            ))
        })
        .await
    }

    pub async fn get_channel_participant_details(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
    ) -> Result<Vec<proto::ChannelMember>> {
        let (role, members) = self
            .transaction(move |tx| async move {
                let role = self
                    .check_user_is_channel_participant(channel_id, user_id, &*tx)
                    .await?;
                Ok((
                    role,
                    self.get_channel_participant_details_internal(channel_id, &*tx)
                        .await?,
                ))
            })
            .await?;

        if role == ChannelRole::Admin {
            Ok(members
                .into_iter()
                .map(|channel_member| channel_member.to_proto())
                .collect())
        } else {
            return Ok(members
                .into_iter()
                .filter_map(|member| {
                    if member.kind == proto::channel_member::Kind::Invitee {
                        return None;
                    }
                    Some(ChannelMember {
                        role: member.role,
                        user_id: member.user_id,
                        kind: proto::channel_member::Kind::Member,
                    })
                })
                .map(|channel_member| channel_member.to_proto())
                .collect());
        }
    }

    async fn get_channel_participant_details_internal(
        &self,
        channel_id: ChannelId,
        tx: &DatabaseTransaction,
    ) -> Result<Vec<ChannelMember>> {
        let channel_visibility = channel::Entity::find()
            .filter(channel::Column::Id.eq(channel_id))
            .one(&*tx)
            .await?
            .map(|channel| channel.visibility)
            .unwrap_or(ChannelVisibility::Members);

        #[derive(Copy, Clone, Debug, EnumIter, DeriveColumn)]
        enum QueryMemberDetails {
            UserId,
            Role,
            IsDirectMember,
            Accepted,
            Visibility,
        }

        let tx = tx;
        let ancestor_ids = self.get_channel_ancestors(channel_id, &*tx).await?;
        let mut stream = channel_member::Entity::find()
            .left_join(channel::Entity)
            .filter(channel_member::Column::ChannelId.is_in(ancestor_ids.iter().copied()))
            .select_only()
            .column(channel_member::Column::UserId)
            .column(channel_member::Column::Role)
            .column_as(
                channel_member::Column::ChannelId.eq(channel_id),
                QueryMemberDetails::IsDirectMember,
            )
            .column(channel_member::Column::Accepted)
            .column(channel::Column::Visibility)
            .into_values::<_, QueryMemberDetails>()
            .stream(&*tx)
            .await?;

        let mut user_details: HashMap<UserId, ChannelMember> = HashMap::default();

        while let Some(user_membership) = stream.next().await {
            let (user_id, channel_role, is_direct_member, is_invite_accepted, visibility): (
                UserId,
                ChannelRole,
                bool,
                bool,
                ChannelVisibility,
            ) = user_membership?;
            let kind = match (is_direct_member, is_invite_accepted) {
                (true, true) => proto::channel_member::Kind::Member,
                (true, false) => proto::channel_member::Kind::Invitee,
                (false, true) => proto::channel_member::Kind::AncestorMember,
                (false, false) => continue,
            };

            if channel_role == ChannelRole::Guest
                && visibility != ChannelVisibility::Public
                && channel_visibility != ChannelVisibility::Public
            {
                continue;
            }

            if let Some(details_mut) = user_details.get_mut(&user_id) {
                if channel_role.should_override(details_mut.role) {
                    details_mut.role = channel_role;
                }
                if kind == Kind::Member {
                    details_mut.kind = kind;
                // the UI is going to be a bit confusing if you already have permissions
                // that are greater than or equal to the ones you're being invited to.
                } else if kind == Kind::Invitee && details_mut.kind == Kind::AncestorMember {
                    details_mut.kind = kind;
                }
            } else {
                user_details.insert(
                    user_id,
                    ChannelMember {
                        user_id,
                        kind,
                        role: channel_role,
                    },
                );
            }
        }

        Ok(user_details
            .into_iter()
            .map(|(_, details)| details)
            .collect())
    }

    pub async fn get_channel_participants(
        &self,
        channel_id: ChannelId,
        tx: &DatabaseTransaction,
    ) -> Result<Vec<UserId>> {
        let participants = self
            .get_channel_participant_details_internal(channel_id, &*tx)
            .await?;
        Ok(participants
            .into_iter()
            .map(|member| member.user_id)
            .collect())
    }

    pub async fn check_user_is_channel_admin(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        tx: &DatabaseTransaction,
    ) -> Result<ChannelRole> {
        let role = self.channel_role_for_user(channel_id, user_id, tx).await?;
        match role {
            Some(ChannelRole::Admin) => Ok(role.unwrap()),
            Some(ChannelRole::Member)
            | Some(ChannelRole::Banned)
            | Some(ChannelRole::Guest)
            | None => Err(anyhow!(
                "user is not a channel admin or channel does not exist"
            ))?,
        }
    }

    pub async fn check_user_is_channel_member(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        tx: &DatabaseTransaction,
    ) -> Result<ChannelRole> {
        let channel_role = self.channel_role_for_user(channel_id, user_id, tx).await?;
        match channel_role {
            Some(ChannelRole::Admin) | Some(ChannelRole::Member) => Ok(channel_role.unwrap()),
            Some(ChannelRole::Banned) | Some(ChannelRole::Guest) | None => Err(anyhow!(
                "user is not a channel member or channel does not exist"
            ))?,
        }
    }

    pub async fn check_user_is_channel_participant(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        tx: &DatabaseTransaction,
    ) -> Result<ChannelRole> {
        let role = self.channel_role_for_user(channel_id, user_id, tx).await?;
        match role {
            Some(ChannelRole::Admin) | Some(ChannelRole::Member) | Some(ChannelRole::Guest) => {
                Ok(role.unwrap())
            }
            Some(ChannelRole::Banned) | None => Err(anyhow!(
                "user is not a channel participant or channel does not exist"
            ))?,
        }
    }

    pub async fn pending_invite_for_channel(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        tx: &DatabaseTransaction,
    ) -> Result<Option<channel_member::Model>> {
        let channel_ids = self.get_channel_ancestors(channel_id, tx).await?;

        let row = channel_member::Entity::find()
            .filter(channel_member::Column::ChannelId.is_in(channel_ids))
            .filter(channel_member::Column::UserId.eq(user_id))
            .filter(channel_member::Column::Accepted.eq(false))
            .one(&*tx)
            .await?;

        Ok(row)
    }

    pub async fn parent_channel_id(
        &self,
        channel_id: ChannelId,
        tx: &DatabaseTransaction,
    ) -> Result<Option<ChannelId>> {
        let path = self.path_to_channel(channel_id, &*tx).await?;
        if path.len() >= 2 {
            Ok(Some(path[path.len() - 2]))
        } else {
            Ok(None)
        }
    }

    pub async fn public_parent_channel_id(
        &self,
        channel_id: ChannelId,
        tx: &DatabaseTransaction,
    ) -> Result<Option<ChannelId>> {
        let path = self.public_path_to_channel(channel_id, &*tx).await?;
        if path.len() >= 2 && path.last().copied() == Some(channel_id) {
            Ok(Some(path[path.len() - 2]))
        } else {
            Ok(path.last().copied())
        }
    }

    pub async fn path_to_channel(
        &self,
        channel_id: ChannelId,
        tx: &DatabaseTransaction,
    ) -> Result<Vec<ChannelId>> {
        let arbitary_path = channel_path::Entity::find()
            .filter(channel_path::Column::ChannelId.eq(channel_id))
            .order_by(channel_path::Column::IdPath, sea_orm::Order::Desc)
            .one(tx)
            .await?;

        let Some(path) = arbitary_path else {
            return Ok(vec![]);
        };

        Ok(path
            .id_path
            .trim_matches('/')
            .split('/')
            .map(|id| ChannelId::from_proto(id.parse().unwrap()))
            .collect())
    }

    pub async fn public_path_to_channel(
        &self,
        channel_id: ChannelId,
        tx: &DatabaseTransaction,
    ) -> Result<Vec<ChannelId>> {
        let ancestor_ids = self.path_to_channel(channel_id, &*tx).await?;

        let rows = channel::Entity::find()
            .filter(channel::Column::Id.is_in(ancestor_ids.iter().copied()))
            .filter(channel::Column::Visibility.eq(ChannelVisibility::Public))
            .all(&*tx)
            .await?;

        let mut visible_channels: HashSet<ChannelId> = HashSet::default();

        for row in rows {
            visible_channels.insert(row.id);
        }

        Ok(ancestor_ids
            .into_iter()
            .filter(|id| visible_channels.contains(id))
            .collect())
    }

    pub async fn channel_role_for_user(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        tx: &DatabaseTransaction,
    ) -> Result<Option<ChannelRole>> {
        let channel_ids = self.get_channel_ancestors(channel_id, tx).await?;

        #[derive(Copy, Clone, Debug, EnumIter, DeriveColumn)]
        enum QueryChannelMembership {
            ChannelId,
            Role,
            Visibility,
        }

        let mut rows = channel_member::Entity::find()
            .left_join(channel::Entity)
            .filter(
                channel_member::Column::ChannelId
                    .is_in(channel_ids)
                    .and(channel_member::Column::UserId.eq(user_id))
                    .and(channel_member::Column::Accepted.eq(true)),
            )
            .select_only()
            .column(channel_member::Column::ChannelId)
            .column(channel_member::Column::Role)
            .column(channel::Column::Visibility)
            .into_values::<_, QueryChannelMembership>()
            .stream(&*tx)
            .await?;

        let mut user_role: Option<ChannelRole> = None;

        let mut is_participant = false;
        let mut current_channel_visibility = None;

        // note these channels are not iterated in any particular order,
        // our current logic takes the highest permission available.
        while let Some(row) = rows.next().await {
            let (membership_channel, role, visibility): (
                ChannelId,
                ChannelRole,
                ChannelVisibility,
            ) = row?;

            match role {
                ChannelRole::Admin | ChannelRole::Member | ChannelRole::Banned => {
                    if let Some(users_role) = user_role {
                        user_role = Some(users_role.max(role));
                    } else {
                        user_role = Some(role)
                    }
                }
                ChannelRole::Guest if visibility == ChannelVisibility::Public => {
                    is_participant = true
                }
                ChannelRole::Guest => {}
            }
            if channel_id == membership_channel {
                current_channel_visibility = Some(visibility);
            }
        }
        // free up database connection
        drop(rows);

        if is_participant && user_role.is_none() {
            if current_channel_visibility.is_none() {
                current_channel_visibility = channel::Entity::find()
                    .filter(channel::Column::Id.eq(channel_id))
                    .one(&*tx)
                    .await?
                    .map(|channel| channel.visibility);
            }
            if current_channel_visibility == Some(ChannelVisibility::Public) {
                user_role = Some(ChannelRole::Guest);
            }
        }

        Ok(user_role)
    }

    /// Returns the channel ancestors in arbitrary order
    pub async fn get_channel_ancestors(
        &self,
        channel_id: ChannelId,
        tx: &DatabaseTransaction,
    ) -> Result<Vec<ChannelId>> {
        let paths = channel_path::Entity::find()
            .filter(channel_path::Column::ChannelId.eq(channel_id))
            .order_by(channel_path::Column::IdPath, sea_orm::Order::Desc)
            .all(tx)
            .await?;
        let mut channel_ids = Vec::new();
        for path in paths {
            for id in path.id_path.trim_matches('/').split('/') {
                if let Ok(id) = id.parse() {
                    let id = ChannelId::from_proto(id);
                    if let Err(ix) = channel_ids.binary_search(&id) {
                        channel_ids.insert(ix, id);
                    }
                }
            }
        }
        Ok(channel_ids)
    }

    // Returns the channel desendants as a sorted list of edges for further processing.
    // The edges are sorted such that you will see unknown channel ids as children
    // before you see them as parents.
    async fn get_channel_descendants(
        &self,
        channel_ids: impl IntoIterator<Item = ChannelId>,
        tx: &DatabaseTransaction,
    ) -> Result<Vec<ChannelEdge>> {
        let mut values = String::new();
        for id in channel_ids {
            if !values.is_empty() {
                values.push_str(", ");
            }
            write!(&mut values, "({})", id).unwrap();
        }

        if values.is_empty() {
            return Ok(vec![]);
        }

        let sql = format!(
            r#"
            SELECT
                descendant_paths.*
            FROM
                channel_paths parent_paths, channel_paths descendant_paths
            WHERE
                parent_paths.channel_id IN ({values}) AND
                descendant_paths.id_path != parent_paths.id_path AND
                descendant_paths.id_path LIKE (parent_paths.id_path || '%')
            ORDER BY
                descendant_paths.id_path
        "#
        );

        let stmt = Statement::from_string(self.pool.get_database_backend(), sql);

        let mut paths = channel_path::Entity::find()
            .from_raw_sql(stmt)
            .stream(tx)
            .await?;

        let mut results: Vec<ChannelEdge> = Vec::new();
        while let Some(path) = paths.next().await {
            let path = path?;
            let ids: Vec<&str> = path.id_path.trim_matches('/').split('/').collect();

            debug_assert!(ids.len() >= 2);
            debug_assert!(ids[ids.len() - 1] == path.channel_id.to_string());

            results.push(ChannelEdge {
                parent_id: ids[ids.len() - 2].parse().unwrap(),
                channel_id: ids[ids.len() - 1].parse().unwrap(),
            })
        }

        Ok(results)
    }

    /// Returns the channel with the given ID
    pub async fn get_channel(&self, channel_id: ChannelId, user_id: UserId) -> Result<Channel> {
        self.transaction(|tx| async move {
            let role = self
                .check_user_is_channel_participant(channel_id, user_id, &*tx)
                .await?;

            let channel = channel::Entity::find_by_id(channel_id).one(&*tx).await?;
            let Some(channel) = channel else {
                Err(anyhow!("no such channel"))?
            };

            Ok(Channel {
                id: channel.id,
                visibility: channel.visibility,
                role,
                name: channel.name,
            })
        })
        .await
    }

    pub(crate) async fn get_or_create_channel_room(
        &self,
        channel_id: ChannelId,
        live_kit_room: &str,
        environment: &str,
        tx: &DatabaseTransaction,
    ) -> Result<RoomId> {
        let room = room::Entity::find()
            .filter(room::Column::ChannelId.eq(channel_id))
            .one(&*tx)
            .await?;

        let room_id = if let Some(room) = room {
            if let Some(env) = room.enviroment {
                if &env != environment {
                    Err(anyhow!("must join using the {} release", env))?;
                }
            }
            room.id
        } else {
            let result = room::Entity::insert(room::ActiveModel {
                channel_id: ActiveValue::Set(Some(channel_id)),
                live_kit_room: ActiveValue::Set(live_kit_room.to_string()),
                enviroment: ActiveValue::Set(Some(environment.to_string())),
                ..Default::default()
            })
            .exec(&*tx)
            .await?;

            result.last_insert_id
        };

        Ok(room_id)
    }

    // Insert an edge from the given channel to the given other channel.
    pub async fn link_channel(
        &self,
        user: UserId,
        channel: ChannelId,
        to: ChannelId,
    ) -> Result<ChannelGraph> {
        self.transaction(|tx| async move {
            self.check_user_is_channel_admin(channel, user, &*tx)
                .await?;

            self.link_channel_internal(user, channel, to, &*tx).await
        })
        .await
    }

    pub async fn link_channel_internal(
        &self,
        user: UserId,
        channel: ChannelId,
        new_parent: ChannelId,
        tx: &DatabaseTransaction,
    ) -> Result<ChannelGraph> {
        self.check_user_is_channel_admin(new_parent, user, &*tx)
            .await?;

        let paths = channel_path::Entity::find()
            .filter(channel_path::Column::IdPath.like(&format!("%/{}/%", channel)))
            .all(tx)
            .await?;

        let mut new_path_suffixes = HashSet::default();
        for path in paths {
            if let Some(start_offset) = path.id_path.find(&format!("/{}/", channel)) {
                new_path_suffixes.insert((
                    path.channel_id,
                    path.id_path[(start_offset + 1)..].to_string(),
                ));
            }
        }

        let paths_to_new_parent = channel_path::Entity::find()
            .filter(channel_path::Column::ChannelId.eq(new_parent))
            .all(tx)
            .await?;

        let mut new_paths = Vec::new();
        for path in paths_to_new_parent {
            if path.id_path.contains(&format!("/{}/", channel)) {
                Err(anyhow!("cycle"))?;
            }

            new_paths.extend(new_path_suffixes.iter().map(|(channel_id, path_suffix)| {
                channel_path::ActiveModel {
                    channel_id: ActiveValue::Set(*channel_id),
                    id_path: ActiveValue::Set(format!("{}{}", &path.id_path, path_suffix)),
                }
            }));
        }

        channel_path::Entity::insert_many(new_paths)
            .exec(&*tx)
            .await?;

        // remove any root edges for the channel we just linked
        {
            channel_path::Entity::delete_many()
                .filter(channel_path::Column::IdPath.like(&format!("/{}/%", channel)))
                .exec(&*tx)
                .await?;
        }

        let mut channel_info = self.get_user_channels(user, Some(channel), &*tx).await?;

        channel_info.channels.edges.push(ChannelEdge {
            channel_id: channel.to_proto(),
            parent_id: new_parent.to_proto(),
        });

        Ok(channel_info.channels)
    }

    /// Unlink a channel from a given parent. This will add in a root edge if
    /// the channel has no other parents after this operation.
    pub async fn unlink_channel(
        &self,
        user: UserId,
        channel: ChannelId,
        from: ChannelId,
    ) -> Result<()> {
        self.transaction(|tx| async move {
            self.check_user_is_channel_admin(channel, user, &*tx)
                .await?;

            self.unlink_channel_internal(user, channel, from, &*tx)
                .await?;

            Ok(())
        })
        .await
    }

    pub async fn unlink_channel_internal(
        &self,
        user: UserId,
        channel: ChannelId,
        from: ChannelId,
        tx: &DatabaseTransaction,
    ) -> Result<()> {
        self.check_user_is_channel_admin(from, user, &*tx).await?;

        let sql = r#"
            DELETE FROM channel_paths
            WHERE
                id_path LIKE '%/' || $1 || '/' || $2 || '/%'
            RETURNING id_path, channel_id
        "#;

        let paths = channel_path::Entity::find()
            .from_raw_sql(Statement::from_sql_and_values(
                self.pool.get_database_backend(),
                sql,
                [from.to_proto().into(), channel.to_proto().into()],
            ))
            .all(&*tx)
            .await?;

        let is_stranded = channel_path::Entity::find()
            .filter(channel_path::Column::ChannelId.eq(channel))
            .count(&*tx)
            .await?
            == 0;

        // Make sure that there is always at least one path to the channel
        if is_stranded {
            let root_paths: Vec<_> = paths
                .iter()
                .map(|path| {
                    let start_offset = path.id_path.find(&format!("/{}/", channel)).unwrap();
                    channel_path::ActiveModel {
                        channel_id: ActiveValue::Set(path.channel_id),
                        id_path: ActiveValue::Set(path.id_path[start_offset..].to_string()),
                    }
                })
                .collect();

            channel_path::Entity::insert_many(root_paths)
                .exec(&*tx)
                .await?;
        }

        Ok(())
    }

    /// Move a channel from one parent to another
    pub async fn move_channel(
        &self,
        channel_id: ChannelId,
        old_parent_id: Option<ChannelId>,
        new_parent_id: ChannelId,
        admin_id: UserId,
    ) -> Result<Option<MoveChannelResult>> {
        self.transaction(|tx| async move {
            self.check_user_is_channel_admin(channel_id, admin_id, &*tx)
                .await?;

            debug_assert_eq!(
                self.parent_channel_id(channel_id, &*tx).await?,
                old_parent_id
            );

            if old_parent_id == Some(new_parent_id) {
                return Ok(None);
            }
            let previous_participants = self
                .get_channel_participant_details_internal(channel_id, &*tx)
                .await?;

            self.link_channel_internal(admin_id, channel_id, new_parent_id, &*tx)
                .await?;

            if let Some(from) = old_parent_id {
                self.unlink_channel_internal(admin_id, channel_id, from, &*tx)
                    .await?;
            }

            let participants_to_update: HashMap<UserId, ChannelsForUser> = self
                .participants_to_notify_for_channel_change(new_parent_id, &*tx)
                .await?
                .into_iter()
                .collect();

            let mut moved_channels: HashSet<ChannelId> = HashSet::default();
            moved_channels.insert(channel_id);
            for edge in self.get_channel_descendants([channel_id], &*tx).await? {
                moved_channels.insert(ChannelId::from_proto(edge.channel_id));
            }

            let mut participants_to_remove: HashSet<UserId> = HashSet::default();
            for participant in previous_participants {
                if participant.kind == proto::channel_member::Kind::AncestorMember {
                    if !participants_to_update.contains_key(&participant.user_id) {
                        participants_to_remove.insert(participant.user_id);
                    }
                }
            }

            Ok(Some(MoveChannelResult {
                participants_to_remove,
                participants_to_update,
                moved_channels,
            }))
        })
        .await
    }
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveColumn)]
enum QueryUserIds {
    UserId,
}

#[derive(Debug)]
pub struct ChannelGraph {
    pub channels: Vec<Channel>,
    pub edges: Vec<ChannelEdge>,
}

impl ChannelGraph {
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty() && self.edges.is_empty()
    }
}

#[cfg(test)]
impl PartialEq for ChannelGraph {
    fn eq(&self, other: &Self) -> bool {
        // Order independent comparison for tests
        let channels_set = self.channels.iter().collect::<HashSet<_>>();
        let other_channels_set = other.channels.iter().collect::<HashSet<_>>();
        let edges_set = self
            .edges
            .iter()
            .map(|edge| (edge.channel_id, edge.parent_id))
            .collect::<HashSet<_>>();
        let other_edges_set = other
            .edges
            .iter()
            .map(|edge| (edge.channel_id, edge.parent_id))
            .collect::<HashSet<_>>();

        channels_set == other_channels_set && edges_set == other_edges_set
    }
}

#[cfg(not(test))]
impl PartialEq for ChannelGraph {
    fn eq(&self, other: &Self) -> bool {
        self.channels == other.channels && self.edges == other.edges
    }
}
