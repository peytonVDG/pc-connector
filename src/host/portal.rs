//! Wayland screen capture through the xdg-desktop-portal ScreenCast interface.
//! The portal hands us a PipeWire node that GStreamer's `pipewiresrc` reads.

use std::os::fd::{AsRawFd, OwnedFd};

use anyhow::{Context, Result, anyhow};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};

pub struct PortalSession {
    runtime: tokio::runtime::Runtime,
    session: Option<Session<Screencast>>,
    _proxy: Screencast,
    fd: OwnedFd,
    pub node_id: u32,
    pub size: Option<(u32, u32)>,
}

impl PortalSession {
    /// Ask the desktop for a screen. Shows the desktop's "share your screen" dialog unless
    /// `restore_token` is accepted. Returns the session and a token for next time.
    pub fn open(restore_token: Option<&str>, show_cursor: bool) -> Result<(PortalSession, Option<String>)> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().context("tokio runtime")?;
        let (proxy, session, fd, node_id, size, token) = runtime.block_on(async {
            let proxy = Screencast::new().await.context("ScreenCast portal not available")?;
            let session = proxy.create_session(Default::default()).await?;
            let cursor = if show_cursor { CursorMode::EMBEDDED } else { CursorMode::HIDDEN };
            let available = proxy.available_cursor_modes().await.unwrap_or(CursorMode::EMBEDDED);
            let cursor = if available.contains(cursor) { cursor } else { CursorMode::HIDDEN };
            proxy
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(cursor)
                        .set_sources(SourceType::MONITOR)
                        .set_multiple(false)
                        .set_persist_mode(PersistMode::ExplicitlyRevoked)
                        .set_restore_token(restore_token),
                )
                .await?;
            log::info!("Waiting for screen-share permission (check the host's screen if a dialog appeared)...");
            let response = proxy.start(&session, None, Default::default()).await?.response()?;
            let stream = response.streams().first().cloned().ok_or_else(|| anyhow!("portal returned no screen"))?;
            let token = response.restore_token().map(str::to_string);
            let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
            let size = stream.size().map(|(w, h)| (w.max(0) as u32, h.max(0) as u32));
            anyhow::Ok((proxy, session, fd, stream.pipe_wire_node_id(), size, token))
        })?;
        log::info!("Portal screen cast ready: node {node_id}, size {size:?}");
        Ok((PortalSession { runtime, session: Some(session), _proxy: proxy, fd, node_id, size }, token))
    }

    pub fn fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = self.runtime.block_on(session.close());
        }
    }
}
