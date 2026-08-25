//! Settings → Mobile: the one place the mobile bridge is turned on, pointed at a
//! port, and paired with a phone.
//!
//! ## The rule this pane is built on
//!
//! Every line that describes what is happening comes from `bridge_status` — what
//! the DAEMON reports — and never from the four stored fields. Those record what
//! the user asked for; only the status records what actually happened. A toggle
//! that reads On while nothing is listening, or a pairing card naming an address
//! nothing bound, is precisely the failure this pane exists to prevent, and the
//! only reliable way to prevent it is to never re-derive the answer locally.
//!
//! That is also why every setter pushes to the daemon and adopts the reply rather
//! than writing the store and repainting. A stored value the live bridge never
//! received is a phone that breaks with nothing on screen to explain it.

use gpui::prelude::FluentBuilder;
use gpui::*;

use crate::bridgecfg;
use crate::qr::Qr;
use crate::settings::{setting_radio_row, setting_toggle_row, settings_body, settings_section};
use crate::*;
use orchestrator_daemon::HostMode;
use orchestrator_host::protocol::{BridgePhase, BridgeStatus};

/// Module pixel size for the pairing QR. 5px keeps a 41×41 symbol at ~205px —
/// big enough that a phone camera locks on immediately at arm's length.
const QR_MODULE: f32 = 5.0;
/// The quiet zone is part of the spec, not padding: decoders need 4 clear modules
/// on every side and will simply not see a symbol drawn flush to its container.
const QR_QUIET: f32 = 4.0;

impl Orchestrator {
    pub(crate) fn render_settings_mobile(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let st = &self.bridge_status;
        let phase = st.phase();
        let daemon_hosted = matches!(self.host_mode, HostMode::Daemon);

        settings_body()
            .child(self.mobile_card_switch(daemon_hosted, phase, cx))
            .child(self.mobile_card_access(cx))
            .child(self.mobile_card_port(cx))
            .child(self.mobile_card_pairing(cx))
    }

    /// Card 1 — the switch, and the truth about what it did.
    fn mobile_card_switch(
        &self,
        daemon_hosted: bool,
        phase: BridgePhase,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let st = self.bridge_status.clone();
        settings_section(
            "Read your sessions from your phone",
            "Kod's session daemon serves a read-only view. It keeps running after you close \
             or quit Kod — that is the point, and it also means it keeps listening until you \
             turn it off here.",
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(setting_toggle_row(
                    "bridge-on",
                    self.bridge_on,
                    "Serve my sessions to my phone",
                    if daemon_hosted {
                        "Off by default. Turning it on mints an access token and starts listening."
                    } else {
                        "Unavailable — Kod is running without its session daemon, so there is no \
                         process that would outlive the app."
                    },
                    cx.listener(move |this: &mut Orchestrator, _, _w, cx| {
                        this.toggle_bridge(cx);
                    }),
                ))
                .child(status_line(&st, phase)),
        )
    }

    /// Card 2 — who can reach it. Selection is derived from what is BOUND.
    fn mobile_card_access(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Derived from the daemon's endpoints, not from `bridge_bind`. A radio
        // computed from a stored key can render narrower than reality — click
        // "This Mac only", fail to restart the listener, and the dot moves while a
        // tailnet socket stays open. Computed from what is actually bound, that
        // state cannot be drawn.
        let reachable = bridgecfg::reachable_host(&self.bridge_status.endpoints);
        let (loopback_selected, tailnet_selected) = if self.bridge_status.running {
            (reachable.is_none(), reachable.is_some())
        } else {
            (self.bridge_bind.is_empty(), !self.bridge_bind.is_empty())
        };
        let tailnet = self.tailnet_ip;
        let tailnet_note = match tailnet {
            Some(ip) => format!("Your phone connects over Tailscale to {ip}."),
            None => "Tailscale is not running — start it and reopen this window.".to_string(),
        };

        settings_section(
            "Who can reach it",
            "Kod refuses every other kind of address. This version authenticates with one \
             shared token and has no TLS, so a Wi-Fi or wildcard address would put that token \
             in the clear.",
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(setting_radio_row(
                    "bridge-bind-loopback",
                    loopback_selected,
                    "This Mac only",
                    "Nothing off this machine can connect. Useful with an SSH tunnel.",
                    cx.listener(|this: &mut Orchestrator, _, _w, cx| {
                        this.set_bridge_bind(String::new(), cx);
                    }),
                ))
                .child(setting_radio_row(
                    "bridge-bind-tailnet",
                    tailnet_selected,
                    "This Mac and my Tailscale network",
                    tailnet_note,
                    cx.listener(move |this: &mut Orchestrator, _, _w, cx| {
                        match tailnet {
                            Some(ip) => this.set_bridge_bind(ip.to_string(), cx),
                            None => {
                                this.bridge_err = Some(
                                    "Tailscale is not running on this Mac, so there is no \
                                     address to listen on. Start Tailscale and reopen Settings."
                                        .into(),
                                );
                                cx.notify();
                            }
                        }
                    }),
                )),
        )
    }

    /// Card 3 — the port.
    fn mobile_card_port(&self, cx: &mut Context<Self>) -> impl IntoElement {
        settings_section(
            "Port",
            "Must match the port in the phone app. The pairing code below already carries it, \
             so you only need this if you are typing the connection in by hand.",
            // Unset shows the port actually in effect, not a "default" label: this
            // is the number the user may have to type into the phone.
            self.render_text_setting_row_with_empty(
                "bridge_port",
                "port",
                &self.bridge_port.to_string(),
                cx,
            ),
        )
    }

    /// Card 4 — pairing. Only ever describes something actually reachable.
    fn mobile_card_pairing(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let st = &self.bridge_status;
        let host = bridgecfg::reachable_host(&st.endpoints);
        let token = self.bridge_token.clone();

        let body: AnyElement = if !st.running {
            hint("Turn the bridge on to pair a phone.").into_any_element()
        } else if host.is_none() {
            // Never print a bare 127.0.0.1 under a label that says "Host": typed
            // into an iPhone that names the PHONE's own loopback, and the user
            // ends up debugging Tailscale, the firewall and the token — none of
            // which are wrong.
            problem(
                "Set to “This Mac only”, so your phone cannot reach it. Choose “This Mac and \
                 my Tailscale network” above to pair.",
            )
            .into_any_element()
        } else if token.is_empty() {
            problem("No access token yet — turn the bridge off and on again to mint one.")
                .into_any_element()
        } else {
            let url = bridgecfg::pair_url(host.as_deref().unwrap_or_default(), st_port(st), &token);
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(qr_block(&url))
                .child(hint("Scan this in the Kod app on your phone. It carries the address, \
                             the port and the token together."))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(flat_button("bridge-copy", "Copy pairing link", {
                            let url = url.clone();
                            cx.listener(move |_this: &mut Orchestrator, _, _w, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                            })
                        }))
                        .child(flat_button(
                            "bridge-regen",
                            "Regenerate token",
                            cx.listener(|this: &mut Orchestrator, _, _w, cx| {
                                this.regenerate_bridge_token(cx);
                            }),
                        )),
                )
                .child(hint(
                    "Regenerating signs out every phone immediately — you will need to scan \
                     again on each one.",
                ))
                .into_any_element()
        };

        settings_section("Pair a phone", "", body)
    }
}

/// The port the daemon is actually serving on, read back from its endpoints so it
/// cannot disagree with the listener.
fn st_port(st: &BridgeStatus) -> u16 {
    st.endpoints
        .first()
        .and_then(|e| e.rsplit_once(':'))
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(orchestrator_bridge::ws::DEFAULT_PORT)
}

/// One line saying what is true right now, in the daemon's own words.
fn status_line(st: &BridgeStatus, phase: BridgePhase) -> AnyElement {
    match phase {
        BridgePhase::Running => {
            let where_ = st.endpoints.join(" and ");
            let who = match st.clients {
                0 => "no phone connected".to_string(),
                1 => "1 phone connected".to_string(),
                n => format!("{n} phones connected"),
            };
            good(format!("Listening on {where_} · {who}"))
        }
        // The daemon's error verbatim. It is written as a sentence for exactly
        // this spot, and re-phrasing it here is how a status line starts lying.
        BridgePhase::Failed => problem(
            st.error
                .clone()
                .unwrap_or_else(|| "The bridge could not start.".into()),
        ),
        // NOT "off": nobody chose this. A storage-free daemon that was restarted
        // has no bridge until the app tells it about one.
        BridgePhase::Waiting => hint("Waiting for Kod to send the settings…"),
        BridgePhase::Off => hint("Not listening."),
    }
}

fn qr_block(url: &str) -> AnyElement {
    let Ok(q) = Qr::encode(url) else {
        return problem("Could not build a pairing code for this address.");
    };
    let side = (q.size as f32 + QR_QUIET * 2.0) * QR_MODULE;
    let mut board = div()
        .relative()
        .w(px(side))
        .h(px(side))
        // A QR is read as dark-on-light. It must keep its own light background
        // regardless of the surrounding dark chrome, or no scanner will see it.
        .bg(rgb(0xFFFFFF))
        .rounded(px(6.));
    for (x, y, w) in bridgecfg::dark_runs(&q) {
        board = board.child(
            div()
                .absolute()
                .left(px((x as f32 + QR_QUIET) * QR_MODULE))
                .top(px((y as f32 + QR_QUIET) * QR_MODULE))
                .w(px(w as f32 * QR_MODULE))
                .h(px(QR_MODULE))
                .bg(rgb(0x000000)),
        );
    }
    board.into_any_element()
}

fn flat_button(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px(px(10.))
        .py(px(5.))
        .rounded(px(6.))
        .border_1()
        .border_color(rgb(HAIR))
        .text_size(px(12.))
        .text_color(rgb(TEXT))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(PANEL)))
        .child(label)
        .on_click(on_click)
}

fn hint(text: impl Into<SharedString>) -> AnyElement {
    div()
        .text_size(px(11.5))
        .text_color(rgb(MUTED2))
        .child(text.into())
        .into_any_element()
}

fn good(text: impl Into<SharedString>) -> AnyElement {
    div()
        .text_size(px(11.5))
        .text_color(rgb(0x8FD2AE))
        .child(text.into())
        .into_any_element()
}

fn problem(text: impl Into<SharedString>) -> AnyElement {
    div()
        .text_size(px(11.5))
        .text_color(rgb(0xE68A8A))
        .child(text.into())
        .into_any_element()
}

// ------------------------------------------------------------------ the setters
//
// Every one of these PUSHES to the daemon and adopts the reply. That is the
// difference between a setting and a lie: the daemon is storage-free and learns
// the bridge config only from a push, so a setter that writes the store and
// repaints leaves the live listener running the OLD config until the next
// launch — which may be days away, because the daemon deliberately outlives
// the app.

impl Orchestrator {
    /// Push the current four fields and adopt whatever comes back.
    fn push_bridge(&mut self, cx: &mut Context<Self>) {
        self.bridge_status = self.host.set_bridge(
            self.bridge_on,
            self.bridge_port,
            &self.bridge_bind,
            &self.bridge_token,
        );
        cx.notify();
    }

    fn store_setting(&mut self, key: &str, val: &str) -> Result<(), String> {
        // Checked, unlike the `let _ = store.set_setting(...)` used elsewhere in
        // this file. "I turned remote access off" failing silently is not a bug
        // class worth inheriting for consistency's sake.
        self.store
            .lock()
            .map_err(|_| "the settings database is busy".to_string())
            .and_then(|s| s.set_setting(key, val).map_err(|e| e.to_string()))
    }

    pub(crate) fn toggle_bridge(&mut self, cx: &mut Context<Self>) {
        self.bridge_err = None;
        if !matches!(self.host_mode, HostMode::Daemon) {
            self.bridge_err = Some(
                "Kod is running without its session daemon, so there is no process that could \
                 keep serving your phone after you quit."
                    .into(),
            );
            cx.notify();
            return;
        }
        let turning_on = !self.bridge_on;

        if turning_on {
            if self.bridge_token.is_empty() {
                match bridgecfg::mint_token() {
                    Ok(t) => {
                        if let Err(e) = self.store_setting("bridge_token", &t) {
                            self.bridge_err = Some(format!("Could not save the access token: {e}"));
                            cx.notify();
                            return;
                        }
                        self.bridge_token = t;
                    }
                    Err(e) => {
                        // Never mint-and-start on a failed read: a predictable
                        // token is worse than no bridge.
                        self.bridge_err = Some(e);
                        cx.notify();
                        return;
                    }
                }
            }
            // First enable defaults to the tailnet when there is one. Loopback
            // would be a bridge no phone can reach, on the one click whose whole
            // purpose is off-Mac reading — "This Mac only" stays one click away
            // as a deliberate downgrade.
            if self.bridge_bind.is_empty() {
                if let Some(ip) = self.tailnet_ip {
                    let s = ip.to_string();
                    let _ = self.store_setting("bridge_bind", &s);
                    self.bridge_bind = s;
                }
            }
        }

        self.bridge_on = turning_on;
        self.push_bridge(cx);

        // Persist only what the daemon confirmed. If the bind failed, the toggle
        // snaps back to Off rather than sitting On over a dead listener.
        let ok = self.bridge_status.running || !turning_on;
        if !ok {
            self.bridge_on = false;
        }
        if let Err(e) = self.store_setting("bridge_on", if self.bridge_on { "1" } else { "0" }) {
            self.bridge_err = Some(format!("Could not save the setting: {e}"));
        }
        cx.notify();
    }

    pub(crate) fn set_bridge_bind(&mut self, bind: String, cx: &mut Context<Self>) {
        self.bridge_err = None;
        if bind == self.bridge_bind {
            return;
        }
        self.bridge_bind = bind.clone();
        // Push BEFORE persisting. Narrowing the exposure is the whole reason
        // someone clicks "This Mac only", and a version that only wrote the store
        // would leave the tailnet listener bound and accepting.
        self.push_bridge(cx);
        if let Err(e) = self.store_setting("bridge_bind", &bind) {
            self.bridge_err = Some(format!("Could not save the setting: {e}"));
        }
        cx.notify();
    }

    pub(crate) fn commit_bridge_port(&mut self, val: &str, cx: &mut Context<Self>) {
        self.bridge_err = None;
        // One validator, shared with the kod-bridge CLI, so the app and the
        // command line can never disagree about what a legal port is.
        match orchestrator_bridge::ws::Config::from_parts(
            if self.bridge_token.is_empty() { "placeholder".into() } else { self.bridge_token.clone() },
            val.trim().parse::<u16>().unwrap_or(0),
            &self.bridge_bind,
        ) {
            Ok(cfg) => {
                self.bridge_port = cfg.port;
                self.push_bridge(cx);
                if let Err(e) = self.store_setting("bridge_port", &cfg.port.to_string()) {
                    self.bridge_err = Some(format!("Could not save the setting: {e}"));
                }
            }
            Err(e) => {
                // Store nothing and leave the running bridge alone: a rejected
                // port must not take the phone down as a side effect.
                self.bridge_err = Some(e);
            }
        }
        cx.notify();
    }

    pub(crate) fn regenerate_bridge_token(&mut self, cx: &mut Context<Self>) {
        self.bridge_err = None;
        let fresh = match bridgecfg::mint_token() {
            Ok(t) => t,
            Err(e) => {
                self.bridge_err = Some(e);
                cx.notify();
                return;
            }
        };
        // PUSH FIRST, persist second — and the push is what actually revokes.
        //
        // Without it this writes a new token to the store and repaints, while the
        // running listener keeps validating the OLD one until the next attach.
        // The card promises every phone is signed out; anyone who read the old
        // token off the screen would still be connected, indefinitely, and the
        // phone shows nothing either way. `bridge::apply` takes the
        // stop-then-start path on any change, which hangs up every connection and
        // mints a fresh epoch.
        let previous = std::mem::replace(&mut self.bridge_token, fresh.clone());
        self.push_bridge(cx);
        if self.bridge_status.running || !self.bridge_on {
            if let Err(e) = self.store_setting("bridge_token", &fresh) {
                self.bridge_err = Some(format!(
                    "The new token is live but could not be saved ({e}) — pair again after \
                     restarting Kod."
                ));
            }
        } else {
            // The daemon refused it; keep the credential the phones still hold
            // rather than stranding them behind a token nothing accepts.
            self.bridge_token = previous;
        }
        cx.notify();
    }
}
