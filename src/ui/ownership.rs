//! Workspace ownership actions: claim, release, and the label each row shows.
//!
//! Ownership is a label, not a permission. These helpers only ever send the
//! feature-gated `WorkspaceSetCreatedBy` request, and every entry point is
//! hidden when the server does not advertise `client-identity`.

use gpui::{App, Context, PromptLevel, Window};
use tty7_core::core::machine::{CreatedBy, CreatedByChange};
use tty7_core::daemon::control::feature;

use crate::core::session::WorkspaceId;
use crate::ui::app::Tty7App;
use crate::ui::i18n::{L10nKey, t, t_fmt};

/// Whether this workspace's server can record ownership.
pub(crate) fn available(cx: &App, id: WorkspaceId) -> bool {
    match crate::core::session::WorkspaceStore::remote_ref(cx, id) {
        Some(remote) => crate::ui::remote_connect::HostLinks::get(cx, remote.host_id())
            .is_some_and(|host| {
                let client = host.client();
                client.is_connected() && client.hello().has_feature(feature::CLIENT_IDENTITY)
            }),
        None => crate::ui::local_link::LocalLink::supports(cx, feature::CLIENT_IDENTITY),
    }
}

/// The holder name without the "by" a label adds: preemption copy already says
/// "opened on {by}", so passing the label there produced "opened on by Bob".
pub(crate) fn name(owner: &CreatedBy) -> String {
    if owner.name.is_empty() {
        t(L10nKey::ClientUnknown).to_string()
    } else {
        owner.name.clone()
    }
}

/// A row's ownership label: mine, another client's name, or unowned.
pub(crate) fn label(created_by: Option<&CreatedBy>, my_id: &str) -> String {
    match created_by {
        None => t(L10nKey::SwitcherUnowned).to_string(),
        Some(owner) if !owner.client_id.is_empty() && owner.client_id == my_id => {
            t(L10nKey::SwitcherMine).to_string()
        }
        Some(owner) => {
            let name = if owner.name.is_empty() {
                t(L10nKey::ClientUnknown).to_string()
            } else {
                owner.name.clone()
            };
            t_fmt(L10nKey::SwitcherOwnedBy, &[("name", &name)])
        }
    }
}

impl Tty7App {
    /// Ask before taking over a workspace whose label belongs to someone else.
    pub(crate) fn claim_workspace(
        &mut self,
        id: WorkspaceId,
        created_by: Option<CreatedBy>,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let me = tty7_core::core::client_identity::current();
        let owner = created_by
            .as_ref()
            .filter(|owner| !owner.client_id.is_empty() && owner.client_id != me.id);
        let Some(owner) = owner else {
            self.set_workspace_owner(
                id,
                CreatedByChange::Set(CreatedBy {
                    client_id: me.id,
                    name: me.name,
                }),
                cx,
            );
            return;
        };
        let owner_name = crate::ui::ownership::name(owner);
        let title = t_fmt(L10nKey::SwitcherClaimTitle, &[("name", name)]);
        let body = t_fmt(L10nKey::SwitcherClaimOtherBody, &[("owner", &owner_name)]);
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            Some(&body),
            &crate::ui::confirm_answers(t(L10nKey::SwitcherClaimConfirm), t(L10nKey::Cancel)),
            cx,
        );
        let me = tty7_core::core::client_identity::current();
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                let _ = this.update_in(cx, |this, _window, cx| {
                    this.set_workspace_owner(
                        id,
                        CreatedByChange::Set(CreatedBy {
                            client_id: me.id,
                            name: me.name,
                        }),
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    pub(crate) fn release_workspace(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        self.set_workspace_owner(id, CreatedByChange::Clear, cx);
    }

    fn set_workspace_owner(
        &mut self,
        id: WorkspaceId,
        change: CreatedByChange,
        cx: &mut Context<Self>,
    ) {
        let adopted_remote = self
            .host_snapshots
            .values()
            .find(|snapshot| snapshot.rows.iter().any(|row| row.id == id))
            .map(|snapshot| (snapshot.target.host_id(), id));
        let supported = match adopted_remote {
            Some((host, _)) => {
                crate::ui::remote_connect::HostLinks::get(cx, host).is_some_and(|host| {
                    let client = host.client();
                    client.is_connected() && client.hello().has_feature(feature::CLIENT_IDENTITY)
                })
            }
            None => available(cx, id),
        };
        if !supported {
            log::warn!("cannot change ownership: the server does not support it");
            return;
        }
        let remote = crate::core::session::WorkspaceStore::remote_ref(cx, id);
        let (client, machine_id, host) = match remote {
            Some(remote) => {
                let host_id = remote.host_id();
                let Some(host) = crate::ui::remote_connect::HostLinks::get(cx, host_id) else {
                    log::warn!("cannot change ownership: {id} is not connected");
                    return;
                };
                (host.client().clone(), remote.workspace, host_id)
            }
            None if adopted_remote.is_some() => {
                let (host_id, machine_id) = adopted_remote.expect("checked");
                let Some(host) = crate::ui::remote_connect::HostLinks::get(cx, host_id) else {
                    log::warn!("cannot change ownership: {id} is not connected");
                    return;
                };
                (host.client().clone(), machine_id, host_id)
            }
            None => {
                let Some(client) = crate::ui::local_link::LocalLink::client(cx) else {
                    log::warn!("cannot change ownership: the local daemon is not connected");
                    return;
                };
                (
                    client,
                    crate::ui::tree_sync::tree_workspace_id(cx, id),
                    crate::core::session::HostId::LOCAL,
                )
            }
        };
        let request = tty7_core::daemon::control::ControlRequest::WorkspaceSetCreatedBy {
            workspace: machine_id,
            created_by: change,
        };
        // The server leaves the author out of its own delta; without this the
        // row that asked keeps the old label until some later pull.
        crate::ui::machine_mirror::MachineMirrors::note_workspace_op(cx, host, &request);

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.call(request) })
                .await;
            if let Err(e) = result {
                log::warn!("could not change workspace ownership: {e}");
            }
            let _ = this.update(cx, |_this, cx| cx.notify());
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(id: &str, name: &str) -> CreatedBy {
        CreatedBy {
            client_id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn labels_say_mine_unowned_or_who_owns_it() {
        crate::ui::i18n::set_locale("en");
        assert_eq!(label(None, "client-a"), "unowned");
        assert_eq!(label(Some(&owner("client-a", "Alice")), "client-a"), "mine");
        assert_eq!(label(Some(&owner("client-b", "Bob")), "client-a"), "by Bob");
        assert_eq!(
            label(Some(&owner("client-b", "")), "client-a"),
            "by unknown client"
        );
    }
}
