//! WebRTC media session (DTLS-SRTP + ICE) built on `webrtcbin`.
//!
//! This is the media layer for accounts with `webrtc` enabled. Unlike
//! [`crate::audio::AudioSession`], which is handed already-negotiated RTP
//! parameters by the SIP layer, a WebRTC session *owns* the SDP: `webrtcbin`
//! produces the offer or answer, and `src/sip/glue.c` carries it verbatim (see
//! the `SOFIA_EV_*_SDP` events).
//!
//! ## Codec negotiation
//!
//! One transceiver is offered carrying all three supported codecs, which
//! `webrtcbin` renders as a single m-line:
//!
//! ```text
//! m=audio 9 UDP/TLS/RTP/SAVPF 111 0 8
//! a=rtpmap:111 OPUS/48000   a=rtpmap:0 PCMU/8000   a=rtpmap:8 PCMA/8000
//! ```
//!
//! `webrtcbin` binds **one codec per sink pad**, and a pad's caps are fixed
//! when it is linked — but the peer is what decides the codec. The send chain
//! is therefore built *after* negotiation, once the choice is known:
//!
//! - **as offerer:** offer all three, then read the codec out of the peer's
//!   answer and build the matching encoder chain.
//! - **as answerer:** read the offer, pick a codec, pin the transceiver's
//!   `codec-preferences` to it so the answer narrows to that one m-line
//!   format, then build the chain and answer.
//!
//! Linking a sink pad after `set-remote-description` is supported, which is
//! what makes the offerer case work at all.
//!
//! ## ICE
//!
//! Vanilla ICE, not trickle: SIP has no natural carrier for late candidates,
//! so each offer/answer waits for `ice-gathering-state == complete` and is
//! then read back from the `local-description` property. The original
//! description object does **not** contain the candidates — they are added to
//! the local description during gathering.
//!
//! Everything runs on the GLib main loop, like the rest of the app; promise
//! and notify callbacks arrive on GStreamer threads and are marshalled back
//! with a oneshot channel.

use futures_channel::oneshot;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};

// ── Codecs ───────────────────────────────────────────────────────────────────

/// Audio codecs this client can encode and decode, in preference order.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Codec {
    Opus,
    Pcmu,
    Pcma,
}

/// Preference order used when picking from a peer's offer.
const PREFERRED: [Codec; 3] = [Codec::Opus, Codec::Pcmu, Codec::Pcma];

impl Codec {
    fn encoding_name(self) -> &'static str {
        match self {
            Codec::Opus => "OPUS",
            Codec::Pcmu => "PCMU",
            Codec::Pcma => "PCMA",
        }
    }

    fn clock_rate(self) -> i32 {
        match self {
            Codec::Opus => 48000,
            _ => 8000,
        }
    }

    /// Payload type used in our own offer. PCMU/PCMA are static types from
    /// RFC 3551; Opus needs a dynamic one.
    fn default_pt(self) -> i32 {
        match self {
            Codec::Opus => 111,
            Codec::Pcmu => 0,
            Codec::Pcma => 8,
        }
    }

    fn encoder(self) -> &'static str {
        match self {
            Codec::Opus => "opusenc",
            Codec::Pcmu => "mulawenc",
            Codec::Pcma => "alawenc",
        }
    }

    fn payloader(self) -> &'static str {
        match self {
            Codec::Opus => "rtpopuspay",
            Codec::Pcmu => "rtppcmupay",
            Codec::Pcma => "rtppcmapay",
        }
    }

    fn depayloader(self) -> &'static str {
        match self {
            Codec::Opus => "rtpopusdepay",
            Codec::Pcmu => "rtppcmudepay",
            Codec::Pcma => "rtppcmadepay",
        }
    }

    fn decoder(self) -> &'static str {
        match self {
            Codec::Opus => "opusdec",
            Codec::Pcmu => "mulawdec",
            Codec::Pcma => "alawdec",
        }
    }

    fn from_encoding_name(name: &str) -> Option<Codec> {
        match name.to_ascii_uppercase().as_str() {
            "OPUS" => Some(Codec::Opus),
            "PCMU" => Some(Codec::Pcmu),
            "PCMA" => Some(Codec::Pcma),
            _ => None,
        }
    }

    /// Static payload types carry no `a=rtpmap` obligation (RFC 3551 §6), so a
    /// peer may omit it and identify the codec by payload type alone.
    fn from_static_pt(pt: u8) -> Option<Codec> {
        match pt {
            0 => Some(Codec::Pcmu),
            8 => Some(Codec::Pcma),
            _ => None,
        }
    }

    fn rtp_caps(self, pt: i32) -> gst::Caps {
        gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", self.encoding_name())
            .field("clock-rate", self.clock_rate())
            .field("payload", pt)
            .build()
    }
}

/// Caps listing every codec we support, used as the transceiver's
/// `codec-preferences` so one m-line offers all of them.
fn all_codec_caps() -> gst::Caps {
    let mut caps = gst::Caps::new_empty();
    {
        let caps = caps.get_mut().expect("freshly created caps are unique");
        for codec in PREFERRED {
            caps.append(codec.rtp_caps(codec.default_pt()));
        }
    }
    caps
}

// ── SDP inspection (pure, unit-tested) ───────────────────────────────────────

/// The payload types of the first `m=audio` line, in the order the peer listed
/// them — which is the peer's own preference order.
fn audio_payload_types(sdp: &str) -> Vec<u8> {
    for line in sdp.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("m=audio ") {
            // "<port> <proto> <fmt> <fmt> ..."
            return rest
                .split_whitespace()
                .skip(2)
                .filter_map(|f| f.parse::<u8>().ok())
                .collect();
        }
    }
    Vec::new()
}

/// Map payload type → encoding name from the `a=rtpmap` lines.
fn rtpmap_names(sdp: &str) -> Vec<(u8, String)> {
    sdp.lines()
        .filter_map(|line| {
            let rest = line.trim_end_matches('\r').strip_prefix("a=rtpmap:")?;
            let (pt, params) = rest.split_once(' ')?;
            let name = params.split('/').next()?;
            Some((pt.trim().parse::<u8>().ok()?, name.trim().to_string()))
        })
        .collect()
}

/// Resolve the codec a peer's SDP selects or offers, together with the payload
/// type it uses for it.
///
/// Payload types are considered in the peer's order, and the first one we can
/// handle wins. In an answer that is the single negotiated codec; in an offer
/// it is the peer's most-preferred codec that we also support.
fn resolve_codec(sdp: &str) -> Option<(Codec, i32)> {
    let names = rtpmap_names(sdp);
    for pt in audio_payload_types(sdp) {
        let codec = names
            .iter()
            .find(|(p, _)| *p == pt)
            .and_then(|(_, n)| Codec::from_encoding_name(n))
            .or_else(|| Codec::from_static_pt(pt));
        if let Some(codec) = codec {
            return Some((codec, pt as i32));
        }
    }
    None
}

/// True when the peer's SDP leaves us receiving only — i.e. it will not accept
/// our audio, so there is no point building a send chain.
fn peer_is_recvonly(sdp: &str) -> bool {
    sdp.lines().any(|l| l.trim_end_matches('\r') == "a=recvonly")
}

// ── Session ──────────────────────────────────────────────────────────────────

fn make(name: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(name)
        .build()
        .map_err(|e| format!("element '{name}': {e}"))
}

/// A single WebRTC audio session: one `webrtcbin`, one audio transceiver.
pub struct WebrtcSession {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    /// `volume` on the send path, created with the send chain once the codec
    /// is known — so it does not exist until negotiation completes.
    send_volume: RefCell<Option<gst::Element>>,
    user_muted: Cell<bool>,
    on_hold: Cell<bool>,
    /// Guards against building the send chain twice (e.g. a re-INVITE answer
    /// arriving after the initial one).
    send_built: Cell<bool>,
    _bus_watch: Option<gst::bus::BusWatchGuard>,
}

impl std::fmt::Debug for WebrtcSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebrtcSession").finish_non_exhaustive()
    }
}

impl WebrtcSession {
    /// Build the pipeline and the audio transceiver. No SDP yet.
    pub fn new() -> Result<Self, String> {
        gst::init().map_err(|e| e.to_string())?;

        let pipeline = gst::Pipeline::new();
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .property_from_str("bundle-policy", "max-bundle")
            .build()
            .map_err(|e| {
                format!(
                    "webrtcbin: {e} — install libgstreamer-plugins-bad1.0-dev \
                     and gstreamer1.0-nice"
                )
            })?;
        pipeline
            .add(&webrtc)
            .map_err(|e| format!("add webrtcbin: {e}"))?;

        // One sendrecv audio transceiver offering every codec we support. The
        // send chain is attached later, once the peer has picked one.
        webrtc.emit_by_name::<Option<glib::Object>>(
            "add-transceiver",
            &[
                &gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv,
                &all_codec_caps(),
            ],
        );

        // Incoming media: webrtcbin exposes a src pad once RTP arrives.
        let pipe_weak = pipeline.downgrade();
        webrtc.connect_pad_added(move |_, pad| {
            let Some(pipeline) = pipe_weak.upgrade() else { return };
            if let Err(e) = build_recv_chain(&pipeline, pad) {
                log::error!("webrtc: receive chain: {e}");
            }
        });

        let bus_watch = pipeline.bus().and_then(|bus| {
            bus.add_watch(move |_, msg| {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Error(e) => {
                        log::error!("[webrtc] {}: {:?}", e.error(), e.debug())
                    }
                    MessageView::Warning(w) => {
                        log::warn!("[webrtc] {}: {:?}", w.error(), w.debug())
                    }
                    _ => {}
                }
                glib::ControlFlow::Continue
            })
            .ok()
        });

        pipeline
            .set_state(gst::State::Ready)
            .map_err(|e| format!("webrtc READY: {e:?}"))?;

        Ok(WebrtcSession {
            pipeline,
            webrtc,
            send_volume: RefCell::new(None),
            user_muted: Cell::new(false),
            on_hold: Cell::new(false),
            send_built: Cell::new(false),
            _bus_watch: bus_watch,
        })
    }

    // ── Offerer ──────────────────────────────────────────────────────────

    /// Produce the SDP offer for an outgoing call, with ICE candidates already
    /// gathered.
    pub async fn create_offer(&self) -> Result<String, String> {
        let offer = self.emit_description("create-offer", "offer").await?;
        self.set_local(&offer).await?;
        self.play()?;
        self.gathered_local_sdp().await
    }

    /// Apply the peer's answer and build the send chain for the codec it chose.
    pub fn apply_answer(&self, sdp: &str) -> Result<(), String> {
        let answer = parse_description(sdp, gst_webrtc::WebRTCSDPType::Answer)?;
        self.webrtc
            .emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
        self.attach_send_chain(sdp)
    }

    // ── Answerer ─────────────────────────────────────────────────────────

    /// Apply the peer's offer, pick a codec, and produce the answer.
    pub async fn create_answer(&self, offer_sdp: &str) -> Result<String, String> {
        let offer = parse_description(offer_sdp, gst_webrtc::WebRTCSDPType::Offer)?;
        self.webrtc
            .emit_by_name::<()>("set-remote-description", &[&offer, &None::<gst::Promise>]);

        // Pin the transceiver to the one codec we picked, so the answer
        // narrows to a single m-line format instead of echoing all of them,
        // and restore sendrecv: the transceiver webrtcbin derives from a
        // remote offer starts out recvonly.
        let (codec, pt) = resolve_codec(offer_sdp)
            .ok_or_else(|| "peer offered no codec we support".to_string())?;
        if let Some(t) = self.transceiver() {
            t.set_property("direction", gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv);
            t.set_property("codec-preferences", codec.rtp_caps(pt));
        }
        self.attach_send_chain(offer_sdp)?;

        let answer = self.emit_description("create-answer", "answer").await?;
        self.set_local(&answer).await?;
        self.play()?;
        self.gathered_local_sdp().await
    }

    // ── Renegotiation ────────────────────────────────────────────────────

    /// Produce a re-INVITE offer that puts the call on hold (`sendonly`) or
    /// resumes it (`sendrecv`). The mic is silenced separately by [`set_hold`].
    ///
    /// [`set_hold`]: WebrtcSession::set_hold
    pub async fn renegotiate_hold(&self, hold: bool) -> Result<String, String> {
        if let Some(t) = self.transceiver() {
            let dir = if hold {
                gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly
            } else {
                gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv
            };
            t.set_property("direction", dir);
        }
        let offer = self.emit_description("create-offer", "offer").await?;
        self.set_local(&offer).await?;
        self.gathered_local_sdp().await
    }

    /// Answer a remote re-INVITE.
    pub async fn answer_reinvite(&self, offer_sdp: &str) -> Result<String, String> {
        let offer = parse_description(offer_sdp, gst_webrtc::WebRTCSDPType::Offer)?;
        self.webrtc
            .emit_by_name::<()>("set-remote-description", &[&offer, &None::<gst::Promise>]);
        let answer = self.emit_description("create-answer", "answer").await?;
        self.set_local(&answer).await?;
        self.gathered_local_sdp().await
    }

    // ── Mic control (mirrors AudioSession) ───────────────────────────────

    /// Silence the mic when the user has muted the call or it is on hold.
    fn apply_mute(&self) {
        if let Some(v) = self.send_volume.borrow().as_ref() {
            v.set_property("mute", self.user_muted.get() || self.on_hold.get());
        }
    }

    pub fn set_muted(&self, muted: bool) {
        self.user_muted.set(muted);
        self.apply_mute();
    }

    pub fn set_hold(&self, hold: bool) {
        self.on_hold.set(hold);
        self.apply_mute();
    }

    // ── Internals ────────────────────────────────────────────────────────

    fn transceiver(&self) -> Option<gst::Object> {
        let t = self
            .webrtc
            .emit_by_name::<Option<gst::Object>>("get-transceiver", &[&0i32]);
        if t.is_none() {
            log::warn!("webrtc: no transceiver at index 0");
        }
        t
    }

    fn play(&self) -> Result<(), String> {
        self.pipeline
            .set_state(gst::State::Playing)
            .map(|_| ())
            .map_err(|e| format!("webrtc PLAYING: {e:?}"))
    }

    /// Emit a promise-based signal (`create-offer` / `create-answer`) and await
    /// the description it produces.
    async fn emit_description(
        &self,
        signal: &str,
        key: &'static str,
    ) -> Result<gst_webrtc::WebRTCSessionDescription, String> {
        let (tx, rx) = oneshot::channel();
        let tx = Arc::new(Mutex::new(Some(tx)));
        let promise = gst::Promise::with_change_func(move |reply| {
            let result = match reply {
                Ok(Some(reply)) => reply
                    .value(key)
                    .ok()
                    .and_then(|v| v.get::<gst_webrtc::WebRTCSessionDescription>().ok())
                    .ok_or_else(|| format!("{key} missing from promise reply")),
                Ok(None) => Err(format!("{key}: empty promise reply")),
                Err(e) => Err(format!("{key}: {e:?}")),
            };
            if let Ok(mut slot) = tx.lock() {
                if let Some(tx) = slot.take() {
                    let _ = tx.send(result);
                }
            }
        });
        self.webrtc
            .emit_by_name::<()>(signal, &[&None::<gst::Structure>, &promise]);
        rx.await.map_err(|_| format!("{signal} was cancelled"))?
    }

    async fn set_local(
        &self,
        desc: &gst_webrtc::WebRTCSessionDescription,
    ) -> Result<(), String> {
        self.webrtc
            .emit_by_name::<()>("set-local-description", &[desc, &None::<gst::Promise>]);
        Ok(())
    }

    /// Wait for ICE gathering to finish and return the local description,
    /// which only then carries the candidates.
    async fn gathered_local_sdp(&self) -> Result<String, String> {
        self.await_ice_complete().await;

        let local = self
            .webrtc
            .property::<Option<gst_webrtc::WebRTCSessionDescription>>("local-description")
            .ok_or_else(|| "no local description after gathering".to_string())?;
        Ok(local.sdp().as_text().map_err(|e| e.to_string())?.to_string())
    }

    async fn await_ice_complete(&self) {
        use gst_webrtc::WebRTCICEGatheringState as State;
        if self.webrtc.property::<State>("ice-gathering-state") == State::Complete {
            return;
        }
        let (tx, rx) = oneshot::channel();
        let tx = Arc::new(Mutex::new(Some(tx)));
        let handler = self
            .webrtc
            .connect_notify(Some("ice-gathering-state"), move |el, _| {
                if el.property::<State>("ice-gathering-state") != State::Complete {
                    return;
                }
                if let Ok(mut slot) = tx.lock() {
                    if let Some(tx) = slot.take() {
                        let _ = tx.send(());
                    }
                }
            });
        // Re-check after connecting: gathering may have completed in the gap
        // between the check above and the handler being installed.
        if self.webrtc.property::<State>("ice-gathering-state") != State::Complete {
            let _ = rx.await;
        }
        self.webrtc.disconnect(handler);
    }

    /// Build the encoder chain for the codec the peer settled on and link it
    /// into `webrtcbin`. Safe to call once per session.
    fn attach_send_chain(&self, peer_sdp: &str) -> Result<(), String> {
        if self.send_built.get() {
            return Ok(());
        }
        if peer_is_recvonly(peer_sdp) {
            log::info!("webrtc: peer is recvonly, not sending audio");
            return Ok(());
        }
        let (codec, pt) = resolve_codec(peer_sdp)
            .ok_or_else(|| "peer selected no codec we support".to_string())?;
        log::info!("webrtc: sending {} (pt {pt})", codec.encoding_name());

        let src = make("autoaudiosrc")?;
        let volume = make("volume")?;
        let convert = make("audioconvert")?;
        let resample = make("audioresample")?;
        // Pin the raw format the encoder expects; Opus runs at 48 kHz while
        // the G.711 encoders require 8 kHz mono.
        let raw_caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("audio/x-raw")
                    .field("rate", codec.clock_rate())
                    .field("channels", 1i32)
                    .build(),
            )
            .build()
            .map_err(|e| format!("raw capsfilter: {e}"))?;
        let encoder = make(codec.encoder())?;
        let payloader = gst::ElementFactory::make(codec.payloader())
            .property("pt", pt as u32)
            .build()
            .map_err(|e| format!("{}: {e}", codec.payloader()))?;
        let rtp_caps = gst::ElementFactory::make("capsfilter")
            .property("caps", codec.rtp_caps(pt))
            .build()
            .map_err(|e| format!("rtp capsfilter: {e}"))?;

        let chain = [
            &src, &volume, &convert, &resample, &raw_caps, &encoder, &payloader, &rtp_caps,
        ];
        for el in chain {
            self.pipeline
                .add(el)
                .map_err(|e| format!("send add: {e}"))?;
        }
        for pair in chain.windows(2) {
            pair[0]
                .link(pair[1])
                .map_err(|e| format!("send link: {e}"))?;
        }

        let sink_pad = self
            .webrtc
            .request_pad_simple("sink_%u")
            .ok_or_else(|| "webrtcbin refused a sink pad".to_string())?;
        rtp_caps
            .static_pad("src")
            .ok_or_else(|| "capsfilter has no src pad".to_string())?
            .link(&sink_pad)
            .map_err(|e| format!("link to webrtcbin: {e}"))?;

        for el in chain {
            el.sync_state_with_parent()
                .map_err(|e| format!("send sync state: {e}"))?;
        }

        *self.send_volume.borrow_mut() = Some(volume);
        self.send_built.set(true);
        self.apply_mute(); // honour a mute/hold set before negotiation finished
        Ok(())
    }
}

impl Drop for WebrtcSession {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        log::info!("webrtc session stopped");
    }
}

/// Decode and play an incoming stream, picking the depayloader from the pad's
/// negotiated caps.
fn build_recv_chain(pipeline: &gst::Pipeline, pad: &gst::Pad) -> Result<(), String> {
    let caps = pad
        .current_caps()
        .or_else(|| pad.allowed_caps())
        .ok_or_else(|| "incoming pad has no caps".to_string())?;
    let s = caps.structure(0).ok_or_else(|| "empty caps".to_string())?;
    if s.name() != "application/x-rtp" {
        return Ok(()); // not media (e.g. a data channel)
    }
    let encoding: String = s
        .get::<String>("encoding-name")
        .map_err(|e| format!("caps without encoding-name: {e}"))?;
    let codec = Codec::from_encoding_name(&encoding)
        .ok_or_else(|| format!("unsupported incoming codec {encoding}"))?;
    log::info!("webrtc: receiving {}", codec.encoding_name());

    let jitter = gst::ElementFactory::make("rtpjitterbuffer")
        .property("latency", 50u32)
        .build()
        .map_err(|e| format!("rtpjitterbuffer: {e}"))?;
    let depay = make(codec.depayloader())?;
    let decode = make(codec.decoder())?;
    let convert = make("audioconvert")?;
    let resample = make("audioresample")?;
    // sync=false for the same reason as the plain-RTP path: remote RTP
    // timestamps start from the peer's own reference and would otherwise
    // stall playback at the start of the call.
    let sink = gst::ElementFactory::make("autoaudiosink")
        .property("sync", false)
        .build()
        .map_err(|e| format!("autoaudiosink: {e}"))?;

    let chain = [&jitter, &depay, &decode, &convert, &resample, &sink];
    for el in chain {
        pipeline.add(el).map_err(|e| format!("recv add: {e}"))?;
    }
    for pair in chain.windows(2) {
        pair[0]
            .link(pair[1])
            .map_err(|e| format!("recv link: {e}"))?;
    }
    for el in chain {
        el.sync_state_with_parent()
            .map_err(|e| format!("recv sync state: {e}"))?;
    }

    let sink_pad = jitter
        .static_pad("sink")
        .ok_or_else(|| "jitterbuffer has no sink pad".to_string())?;
    pad.link(&sink_pad)
        .map_err(|e| format!("link incoming pad: {e}"))?;
    Ok(())
}

fn parse_description(
    sdp: &str,
    kind: gst_webrtc::WebRTCSDPType,
) -> Result<gst_webrtc::WebRTCSessionDescription, String> {
    let msg = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
        .map_err(|e| format!("could not parse remote SDP: {e}"))?;
    Ok(gst_webrtc::WebRTCSessionDescription::new(kind, msg))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\n\
        o=- 1 0 IN IP4 0.0.0.0\r\n\
        s=-\r\nt=0 0\r\n\
        m=audio 9 UDP/TLS/RTP/SAVPF 111 0 8\r\n\
        a=rtpmap:111 OPUS/48000\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:8 PCMA/8000\r\n\
        a=sendrecv\r\n";

    #[test]
    fn reads_the_payload_types_in_peer_order() {
        assert_eq!(audio_payload_types(OFFER), vec![111, 0, 8]);
    }

    #[test]
    fn ignores_video_m_lines() {
        let sdp = "m=video 9 UDP/TLS/RTP/SAVPF 96\r\nm=audio 9 UDP/TLS/RTP/SAVPF 8\r\n";
        assert_eq!(audio_payload_types(sdp), vec![8]);
    }

    #[test]
    fn no_audio_line_yields_nothing() {
        assert!(audio_payload_types("v=0\r\nm=video 9 RTP/AVP 96\r\n").is_empty());
        assert_eq!(resolve_codec("v=0\r\n"), None);
    }

    #[test]
    fn parses_rtpmap_names() {
        assert_eq!(
            rtpmap_names(OFFER),
            vec![
                (111, "OPUS".to_string()),
                (0, "PCMU".to_string()),
                (8, "PCMA".to_string())
            ]
        );
    }

    #[test]
    fn resolves_the_peers_first_supported_codec() {
        assert_eq!(resolve_codec(OFFER), Some((Codec::Opus, 111)));
    }

    #[test]
    fn an_answer_resolves_to_its_single_codec() {
        let answer = "m=audio 9 UDP/TLS/RTP/SAVPF 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
        assert_eq!(resolve_codec(answer), Some((Codec::Pcmu, 0)));
    }

    #[test]
    fn honours_peer_preference_over_ours() {
        // We prefer Opus, but a peer listing PCMA first gets PCMA.
        let sdp = "m=audio 9 UDP/TLS/RTP/SAVPF 8 111\r\n\
                   a=rtpmap:8 PCMA/8000\r\na=rtpmap:111 OPUS/48000\r\n";
        assert_eq!(resolve_codec(sdp), Some((Codec::Pcma, 8)));
    }

    #[test]
    fn skips_codecs_we_cannot_handle() {
        let sdp = "m=audio 9 UDP/TLS/RTP/SAVPF 97 8\r\n\
                   a=rtpmap:97 G722/8000\r\na=rtpmap:8 PCMA/8000\r\n";
        assert_eq!(resolve_codec(sdp), Some((Codec::Pcma, 8)));
    }

    #[test]
    fn static_payload_types_need_no_rtpmap() {
        // RFC 3551 §6: a peer may identify PCMU/PCMA by payload type alone.
        let sdp = "m=audio 9 RTP/AVP 0\r\na=sendrecv\r\n";
        assert_eq!(resolve_codec(sdp), Some((Codec::Pcmu, 0)));
    }

    #[test]
    fn a_dynamic_type_without_rtpmap_is_not_guessed() {
        assert_eq!(resolve_codec("m=audio 9 RTP/AVP 111\r\n"), None);
    }

    #[test]
    fn detects_a_recvonly_peer() {
        assert!(peer_is_recvonly("m=audio 9 RTP/AVP 0\r\na=recvonly\r\n"));
        assert!(!peer_is_recvonly(OFFER));
    }

    #[test]
    fn every_codec_offers_a_distinct_payload_type() {
        let pts: Vec<i32> = PREFERRED.iter().map(|c| c.default_pt()).collect();
        let mut sorted = pts.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(pts.len(), sorted.len(), "payload types collide: {pts:?}");
    }

    #[test]
    fn the_offer_caps_carry_every_codec() {
        gst::init().unwrap();
        let caps = all_codec_caps();
        assert_eq!(caps.size(), PREFERRED.len());
        let names: Vec<String> = (0..caps.size())
            .filter_map(|i| caps.structure(i)?.get::<String>("encoding-name").ok())
            .collect();
        assert_eq!(names, vec!["OPUS", "PCMU", "PCMA"]);
    }

    #[test]
    fn every_codecs_elements_exist_on_this_system() {
        gst::init().unwrap();
        for codec in PREFERRED {
            for name in [
                codec.encoder(),
                codec.payloader(),
                codec.depayloader(),
                codec.decoder(),
            ] {
                assert!(
                    gst::ElementFactory::find(name).is_some(),
                    "missing GStreamer element {name} for {:?}",
                    codec
                );
            }
        }
    }
}

// ── End-to-end negotiation test ──────────────────────────────────────────────

/// Negotiates two real `WebrtcSession`s against each other, so the offer /
/// answer path, codec selection, ICE gathering and the DTLS-SRTP handshake are
/// exercised for real rather than mocked.
#[cfg(test)]
mod e2e {
    use super::*;
    use std::time::{Duration, Instant};

    /// Poll `f` on the main loop until it holds, or fail after `secs`.
    fn wait_until(main: &glib::MainContext, secs: u64, what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            main.iteration(false);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn two_sessions_negotiate_and_connect() {
        // A private context, not the default one: `WebrtcSession` never uses
        // `spawn_local`, so it has no attachment to the default context — and
        // sharing that one across the harness's test threads trips glib's
        // ThreadGuard when a value created by one test is touched by another.
        let main = glib::MainContext::new();
        let _guard = main.acquire().expect("a fresh main context is free");

        let caller = WebrtcSession::new().expect("caller session");
        let callee = WebrtcSession::new().expect("callee session");

        // ── Offer ─────────────────────────────────────────────────────────
        let offer = main.block_on(caller.create_offer()).expect("offer");
        assert_eq!(
            audio_payload_types(&offer),
            vec![111, 0, 8],
            "all three codecs must be offered: {offer}"
        );
        for required in [
            "UDP/TLS/RTP/SAVPF", // DTLS-SRTP profile
            "a=fingerprint:",    // DTLS certificate fingerprint
            "a=ice-ufrag:",
            "a=ice-pwd:",
            "a=rtcp-mux",
            "a=setup:actpass",
            "a=sendrecv",
        ] {
            assert!(offer.contains(required), "offer missing {required}:\n{offer}");
        }
        assert!(
            offer.contains("a=candidate:"),
            "vanilla ICE: candidates must be gathered into the offer:\n{offer}"
        );

        // ── Answer ────────────────────────────────────────────────────────
        let answer = main.block_on(callee.create_answer(&offer)).expect("answer");
        assert_eq!(
            resolve_codec(&answer),
            Some((Codec::Opus, 111)),
            "answer should settle on our first preference: {answer}"
        );
        assert_eq!(
            answer.matches("a=rtpmap").count(),
            1,
            "answer must narrow to one codec:\n{answer}"
        );
        assert!(answer.contains("a=sendrecv"), "answer must be sendrecv:\n{answer}");
        assert!(answer.contains("a=candidate:"), "answer needs candidates:\n{answer}");
        assert!(callee.send_built.get(), "answerer builds its send chain");

        // ── Apply the answer ──────────────────────────────────────────────
        caller.apply_answer(&answer).expect("apply answer");
        assert!(caller.send_built.get(), "offerer builds its send chain");

        // ── DTLS-SRTP actually completes ──────────────────────────────────
        use gst_webrtc::WebRTCPeerConnectionState as State;
        let connected = |s: &WebrtcSession| {
            s.webrtc.property::<State>("connection-state") == State::Connected
        };
        wait_until(&main, 30, "the peer connection", || {
            connected(&caller) && connected(&callee)
        });
    }

    #[test]
    fn a_pcmu_only_peer_is_answered_with_pcmu() {
        // A private context, not the default one: `WebrtcSession` never uses
        // `spawn_local`, so it has no attachment to the default context — and
        // sharing that one across the harness's test threads trips glib's
        // ThreadGuard when a value created by one test is touched by another.
        let main = glib::MainContext::new();
        let _guard = main.acquire().expect("a fresh main context is free");

        // An offer from a peer that only speaks G.711 µ-law — the case that
        // motivated multi-codec support in the first place.
        let caller = WebrtcSession::new().expect("caller session");
        let offer = main.block_on(caller.create_offer()).expect("offer");
        let pcmu_offer: String = offer
            .lines()
            .filter(|l| !l.starts_with("a=rtpmap:111") && !l.starts_with("a=rtpmap:8"))
            .filter(|l| !l.starts_with("a=fmtp:111") && !l.starts_with("a=rtcp-fb:111"))
            .map(|l| {
                if l.starts_with("m=audio") {
                    l.replace("SAVPF 111 0 8", "SAVPF 0")
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\r\n")
            + "\r\n";

        let callee = WebrtcSession::new().expect("callee session");
        let answer = main.block_on(callee.create_answer(&pcmu_offer)).expect("answer");
        assert_eq!(resolve_codec(&answer), Some((Codec::Pcmu, 0)), "{answer}");
    }
}
