use gstreamer as gst;
use gstreamer::prelude::*;
use gtk4::{gio, glib};

pub struct AudioSession {
    send: gst::Pipeline,
    recv: gst::Pipeline,
    // `volume` element on the send path; its `mute` property toggles the mic.
    send_volume: gst::Element,
    // Bus watches remove themselves when their guard is dropped, so they must
    // be held for the lifetime of the session — otherwise pipeline errors and
    // warnings go unreported.
    _bus_watches: Vec<gst::bus::BusWatchGuard>,
}

fn make(name: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(name)
        .build()
        .map_err(|e| format!("element '{name}': {e}"))
}

impl AudioSession {
    pub fn start(
        local_rtp_port: u16,
        remote_ip: &str,
        remote_rtp_port: u16,
        codec: u8,
    ) -> Result<Self, String> {
        gst::init().map_err(|e| e.to_string())?;

        log::info!(
            "audio: codec={codec} local_port={local_rtp_port} \
             remote={remote_ip}:{remote_rtp_port}"
        );

        let (rtp_caps, depay_name, decode_name, enc_name, pay_name, pay_pt) = match codec {
            8 => (
                gst::Caps::builder("application/x-rtp")
                    .field("media", "audio")
                    .field("clock-rate", 8000i32)
                    .field("payload", 8i32)
                    .field("encoding-name", "PCMA")
                    .build(),
                "rtppcmadepay", "alawdec", "alawenc", "rtppcmapay", 8u32,
            ),
            _ => (
                gst::Caps::builder("application/x-rtp")
                    .field("media", "audio")
                    .field("clock-rate", 8000i32)
                    .field("payload", 0i32)
                    .field("encoding-name", "PCMU")
                    .build(),
                "rtppcmudepay", "mulawdec", "mulawenc", "rtppcmupay", 0u32,
            ),
        };

        // ── Receive pipeline ─────────────────────────────────────────────────
        // Build this first so we can get the bound UDP socket from udpsrc and
        // pass it to udpsink, ensuring both use the same local port.  Asterisk
        // (and most SIP servers) use symmetric RTP / comedia: they send RTP
        // back to whatever source port they receive our RTP from, so the send
        // and receive paths must share one socket on local_rtp_port.

        let udpsrc = gst::ElementFactory::make("udpsrc")
            .property("port", local_rtp_port as i32)
            .build()
            .map_err(|e| format!("udpsrc: {e}"))?;

        let caps_filter = gst::ElementFactory::make("capsfilter")
            .property("caps", &rtp_caps)
            .build()
            .map_err(|e| format!("capsfilter: {e}"))?;

        let jitterbuf = gst::ElementFactory::make("rtpjitterbuffer")
            .property("latency", 50u32)
            .build()
            .map_err(|e| format!("rtpjitterbuffer: {e}"))?;

        let depay     = make(depay_name)?;
        let decoder   = make(decode_name)?;
        let aconv_r   = make("audioconvert")?;

        // sync=false: play audio as it arrives without waiting for the pipeline
        // clock to match the RTP timestamps (which start from Asterisk's own
        // session reference and can be tens of seconds ahead of our pipeline's
        // running time, causing a long silence at the start of every call).
        let audio_out = gst::ElementFactory::make("autoaudiosink")
            .property("sync", false)
            .build()
            .map_err(|e| format!("autoaudiosink: {e}"))?;

        let recv = gst::Pipeline::new();
        for el in [&udpsrc, &caps_filter, &jitterbuf, &depay, &decoder, &aconv_r, &audio_out] {
            recv.add(el).map_err(|e| format!("recv add: {e}"))?;
        }
        udpsrc.link(&caps_filter).map_err(|e| format!("udpsrc→caps: {e}"))?;
        caps_filter.link(&jitterbuf).map_err(|e| format!("caps→jbuf: {e}"))?;
        jitterbuf.link(&depay).map_err(|e| format!("jbuf→depay: {e}"))?;
        depay.link(&decoder).map_err(|e| format!("depay→dec: {e}"))?;
        decoder.link(&aconv_r).map_err(|e| format!("dec→conv: {e}"))?;
        aconv_r.link(&audio_out).map_err(|e| format!("conv→sink: {e}"))?;

        // READY causes udpsrc to bind its socket; we then steal it for udpsink.
        recv.set_state(gst::State::Ready)
            .map_err(|e| format!("recv READY: {e:?}"))?;

        let shared_socket = udpsrc.property::<Option<gio::Socket>>("used-socket");
        log::debug!("shared socket acquired: {}", shared_socket.is_some());

        // ── Send pipeline ────────────────────────────────────────────────────

        let audio_in  = make("autoaudiosrc")?;
        // `volume` sits right after the source so muting silences the mic
        // independently of hold (which pauses the whole send pipeline).
        let volume    = make("volume")?;
        let aconv_s   = make("audioconvert")?;
        let aresamp   = make("audioresample")?;

        let raw_caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("audio/x-raw")
                    .field("rate", 8000i32)
                    .field("channels", 1i32)
                    .field("format", "S16LE")
                    .build(),
            )
            .build()
            .map_err(|e| format!("raw capsfilter: {e}"))?;

        let encoder = make(enc_name)?;
        let pay = gst::ElementFactory::make(pay_name)
            .property("pt", pay_pt)
            .build()
            .map_err(|e| format!("{pay_name}: {e}"))?;

        let mut sink_b = gst::ElementFactory::make("udpsink")
            .property("host", remote_ip)
            .property("port", remote_rtp_port as i32);
        if let Some(sock) = shared_socket {
            sink_b = sink_b.property("socket", sock);
        }
        let udpsink = sink_b.build().map_err(|e| format!("udpsink: {e}"))?;

        let send = gst::Pipeline::new();
        for el in [&audio_in, &volume, &aconv_s, &aresamp, &raw_caps, &encoder, &pay, &udpsink] {
            send.add(el).map_err(|e| format!("send add: {e}"))?;
        }
        audio_in.link(&volume).map_err(|e| format!("src→vol: {e}"))?;
        volume.link(&aconv_s).map_err(|e| format!("vol→conv: {e}"))?;
        aconv_s.link(&aresamp).map_err(|e| format!("conv→resamp: {e}"))?;
        aresamp.link(&raw_caps).map_err(|e| format!("resamp→rawcaps: {e}"))?;
        raw_caps.link(&encoder).map_err(|e| format!("rawcaps→enc: {e}"))?;
        encoder.link(&pay).map_err(|e| format!("enc→pay: {e}"))?;
        pay.link(&udpsink).map_err(|e| format!("pay→udpsink: {e}"))?;

        // ── Bus monitoring ───────────────────────────────────────────────────
        // add_watch returns a guard that removes the watch when dropped, so the
        // guards are collected into the session rather than discarded here.
        let mut bus_watches = Vec::new();
        for (label, pipeline) in [("send", &send), ("recv", &recv)] {
            let label = label.to_owned();
            if let Some(bus) = pipeline.bus() {
                if let Ok(guard) = bus.add_watch(move |_, msg| {
                    use gst::MessageView;
                    match msg.view() {
                        MessageView::Error(e) => {
                            log::error!("[audio/{label}] {}: {:?}", e.error(), e.debug());
                        }
                        MessageView::Warning(w) => {
                            log::warn!("[audio/{label}] {}: {:?}", w.error(), w.debug());
                        }
                        _ => {}
                    }
                    glib::ControlFlow::Continue
                }) {
                    bus_watches.push(guard);
                }
            }
        }

        // ── Play ─────────────────────────────────────────────────────────────
        send.set_state(gst::State::Playing)
            .map_err(|e| format!("send PLAY: {e:?}"))?;
        recv.set_state(gst::State::Playing)
            .map_err(|e| format!("recv PLAY: {e:?}"))?;

        Ok(AudioSession { send, recv, send_volume: volume, _bus_watches: bus_watches })
    }

    pub fn set_hold(&self, hold: bool) {
        let state = if hold { gst::State::Paused } else { gst::State::Playing };
        let _ = self.send.set_state(state);
    }

    /// Mute or unmute the outgoing microphone audio.  Independent of hold:
    /// the `volume` element stays in the pipeline, so toggling mute works
    /// whether or not the call is on hold.
    pub fn set_muted(&self, muted: bool) {
        self.send_volume.set_property("mute", muted);
    }
}

impl Drop for AudioSession {
    fn drop(&mut self) {
        let _ = self.send.set_state(gst::State::Null);
        let _ = self.recv.set_state(gst::State::Null);
        log::info!("audio stopped");
    }
}
