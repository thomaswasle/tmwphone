use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gtk4::glib;

/// Plays a looping tone pattern. Dropped automatically when the call connects or ends.
pub struct Ringer {
    pipeline: gst::Pipeline,
    alive: Rc<RefCell<bool>>,
}

/// Advance to the next cadence step after `delay` ms, toggling the volume element.
/// Stops cleanly when `alive` is set to false (i.e. on Ringer::drop).
fn tick(cadence: Rc<Vec<u32>>, step: Rc<RefCell<usize>>, vol: gst::Element, alive: Rc<RefCell<bool>>) {
    let delay = cadence[*step.borrow()] as u64;
    glib::timeout_add_local(Duration::from_millis(delay), move || {
        if !*alive.borrow() {
            return glib::ControlFlow::Break;
        }
        let next = (*step.borrow() + 1) % cadence.len();
        *step.borrow_mut() = next;
        // Even steps are "on", odd steps are "off".
        vol.set_property("volume", if next % 2 == 0 { 0.4f64 } else { 0.0f64 });
        tick(cadence.clone(), step.clone(), vol.clone(), alive.clone());
        glib::ControlFlow::Break
    });
}

impl Ringer {
    /// Incoming call tone: 440 Hz, 0.4 s on / 0.2 s off / 0.4 s on / 2.0 s off.
    pub fn start_incoming() -> Option<Self> {
        Self::build(440.0, &[400, 200, 400, 2000])
    }

    /// Outgoing ringback tone: 425 Hz, 1 s on / 3 s off.
    pub fn start_ringback() -> Option<Self> {
        Self::build(425.0, &[1000, 3000])
    }

    fn build(freq: f64, cadence_ms: &[u32]) -> Option<Self> {
        gst::init().ok()?;

        // No is-live=true: the sink clocks the pipeline so buffers are paced
        // correctly without underruns.  No sync=false on the sink: that flag is
        // only appropriate for RTP (whose timestamps don't match the pipeline
        // clock); on a local tone generator it causes continuous glitching.
        let src = gst::ElementFactory::make("audiotestsrc")
            .property_from_str("wave", "sine")
            .property("freq", freq)
            .build()
            .ok()?;

        let vol = gst::ElementFactory::make("volume")
            .property("volume", 0.4f64)
            .build()
            .ok()?;

        let conv   = gst::ElementFactory::make("audioconvert").build().ok()?;
        let resamp = gst::ElementFactory::make("audioresample").build().ok()?;
        let sink   = gst::ElementFactory::make("autoaudiosink").build().ok()?;

        let pipeline = gst::Pipeline::new();
        pipeline.add_many([&src, &vol, &conv, &resamp, &sink]).ok()?;
        src.link(&vol).ok()?;
        vol.link(&conv).ok()?;
        conv.link(&resamp).ok()?;
        resamp.link(&sink).ok()?;

        pipeline.set_state(gst::State::Playing).ok()?;

        let alive = Rc::new(RefCell::new(true));
        // Step 0 = first "on" interval; pipeline already playing at full volume.
        tick(
            Rc::new(cadence_ms.to_vec()),
            Rc::new(RefCell::new(0usize)),
            vol,
            alive.clone(),
        );

        Some(Ringer { pipeline, alive })
    }
}

impl Drop for Ringer {
    fn drop(&mut self) {
        *self.alive.borrow_mut() = false;
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
