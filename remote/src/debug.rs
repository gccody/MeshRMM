use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use meshrmm_protocol::Codec;

#[derive(Clone)]
pub struct DebugInfo {
    state: Arc<Mutex<DebugState>>,
}

struct DebugState {
    session_id: String,
    started: Instant,
    connection_state: String,
    connection_path: String,
    selected_pair: String,
    local_candidate: String,
    remote_candidate: String,
    control_channel: String,
    video_channel: String,
    display: String,
    stream: String,
    codec: String,
    target_fps: u16,
    received_fps: f64,
    decode_fps: Option<f64>,
    present_fps: Option<f64>,
    rtt_ms: Option<f64>,
    available_incoming_bitrate_bps: Option<f64>,
    receive_bitrate_bps: Option<f64>,
    packets_received: u32,
    bytes_received: u64,
    frames_received: u64,
    incomplete_frames_dropped: u64,
    stale_packets_dropped: u64,
    duplicate_packets: u64,
    invalid_packets: u64,
    presenter_frames_dropped: u64,
    decoded_frames_dropped: u64,
    frames_presented: u64,
    last_encode_ms: f64,
    received_window_started: Instant,
    received_window_frames: u64,
    network_sample: Option<(Instant, u64)>,
}

impl DebugInfo {
    pub fn new(session_id: impl Into<String>) -> Self {
        let now = Instant::now();
        Self {
            state: Arc::new(Mutex::new(DebugState {
                session_id: session_id.into(),
                started: now,
                connection_state: "signaling".into(),
                connection_path: "selecting ICE route".into(),
                selected_pair: "waiting for candidate pair".into(),
                local_candidate: "unknown".into(),
                remote_candidate: "unknown".into(),
                control_channel: "waiting".into(),
                video_channel: "waiting".into(),
                display: "waiting for stream".into(),
                stream: "not configured".into(),
                codec: "not negotiated".into(),
                target_fps: 0,
                received_fps: 0.0,
                decode_fps: None,
                present_fps: None,
                rtt_ms: None,
                available_incoming_bitrate_bps: None,
                receive_bitrate_bps: None,
                packets_received: 0,
                bytes_received: 0,
                frames_received: 0,
                incomplete_frames_dropped: 0,
                stale_packets_dropped: 0,
                duplicate_packets: 0,
                invalid_packets: 0,
                presenter_frames_dropped: 0,
                decoded_frames_dropped: 0,
                frames_presented: 0,
                last_encode_ms: 0.0,
                received_window_started: now,
                received_window_frames: 0,
                network_sample: None,
            })),
        }
    }

    pub fn set_connection_state(&self, state: impl Into<String>) {
        if let Ok(mut debug) = self.state.lock() {
            debug.connection_state = state.into();
        }
    }

    pub fn set_selected_pair(&self, path: impl Into<String>, pair: impl Into<String>) {
        if let Ok(mut debug) = self.state.lock() {
            debug.connection_path = path.into();
            debug.selected_pair = pair.into();
        }
    }

    pub fn set_data_channel(&self, label: &str, state: &str) {
        if let Ok(mut debug) = self.state.lock() {
            match label {
                "meshrmm-control-v1" => debug.control_channel = state.into(),
                "meshrmm-video-v1" => debug.video_channel = state.into(),
                _ => {}
            }
        }
    }

    pub fn configure_stream(
        &self,
        display: impl Into<String>,
        width: u32,
        height: u32,
        frames_per_second: u16,
        codec: Codec,
    ) {
        if let Ok(mut debug) = self.state.lock() {
            debug.display = display.into();
            debug.stream = format!("{width}x{height}");
            debug.codec = match codec {
                Codec::H264 => "H.264 / AVC",
                Codec::H265 => "H.265 / HEVC",
            }
            .into();
            debug.target_fps = frames_per_second;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_network(
        &self,
        rtt_ms: f64,
        available_incoming_bitrate_bps: f64,
        packets_received: u32,
        bytes_received: u64,
        local_candidate: impl Into<String>,
        remote_candidate: impl Into<String>,
        path: impl Into<String>,
    ) {
        if let Ok(mut debug) = self.state.lock() {
            let now = Instant::now();
            if let Some((sampled_at, sampled_bytes)) = debug.network_sample
                && bytes_received >= sampled_bytes
            {
                let elapsed = now.duration_since(sampled_at).as_secs_f64();
                if elapsed > 0.0 {
                    debug.receive_bitrate_bps =
                        Some(bytes_received.saturating_sub(sampled_bytes) as f64 * 8.0 / elapsed);
                }
            }
            debug.network_sample = Some((now, bytes_received));
            debug.rtt_ms = Some(rtt_ms);
            debug.available_incoming_bitrate_bps = Some(available_incoming_bitrate_bps);
            debug.packets_received = packets_received;
            debug.bytes_received = bytes_received;
            debug.local_candidate = local_candidate.into();
            debug.remote_candidate = remote_candidate.into();
            debug.connection_path = path.into();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_received_frame(
        &self,
        encode_us: u64,
        frames_received: u64,
        incomplete_frames_dropped: u64,
        stale_packets_dropped: u64,
        duplicate_packets: u64,
        invalid_packets: u64,
    ) {
        if let Ok(mut debug) = self.state.lock() {
            debug.frames_received = frames_received;
            debug.incomplete_frames_dropped = incomplete_frames_dropped;
            debug.stale_packets_dropped = stale_packets_dropped;
            debug.duplicate_packets = duplicate_packets;
            debug.invalid_packets = invalid_packets;
            debug.last_encode_ms = encode_us as f64 / 1_000.0;
            debug.received_window_frames += 1;
            let elapsed = debug.received_window_started.elapsed();
            if elapsed >= Duration::from_secs(1) {
                debug.received_fps = debug.received_window_frames as f64 / elapsed.as_secs_f64();
                debug.received_window_frames = 0;
                debug.received_window_started = Instant::now();
            }
        }
    }

    pub fn update_presentation(
        &self,
        decode_fps: Option<f64>,
        present_fps: f64,
        frames_presented: u64,
        presenter_frames_dropped: Option<u64>,
        decoded_frames_dropped: u64,
    ) {
        if let Ok(mut debug) = self.state.lock() {
            debug.decode_fps = decode_fps;
            debug.present_fps = Some(present_fps);
            debug.frames_presented = frames_presented;
            if let Some(frames) = presenter_frames_dropped {
                debug.presenter_frames_dropped = frames;
            }
            debug.decoded_frames_dropped = decoded_frames_dropped;
        }
    }

    #[cfg(target_os = "macos")]
    pub fn set_presenter_frames_dropped(&self, frames: u64) {
        if let Ok(mut debug) = self.state.lock() {
            debug.presenter_frames_dropped = frames;
        }
    }

    pub fn render(&self) -> String {
        let Ok(debug) = self.state.lock() else {
            return "MeshRMM debug information unavailable".into();
        };
        let rtt = debug
            .rtt_ms
            .map_or_else(|| "--".into(), |value| format!("{value:.1} ms"));
        let available_bandwidth = debug.available_incoming_bitrate_bps.map_or_else(
            || "--".into(),
            |value| format!("{:.2} Mbps", value / 1_000_000.0),
        );
        let receive_bandwidth = debug.receive_bitrate_bps.map_or_else(
            || "--".into(),
            |value| format!("{:.2} Mbps", value / 1_000_000.0),
        );
        let decode_fps = debug
            .decode_fps
            .map_or_else(|| "--".into(), |value| format!("{value:.1}"));
        let present_fps = debug
            .present_fps
            .map_or_else(|| "--".into(), |value| format!("{value:.1}"));
        format!(
            "MeshRMM diagnostics  [F12 to close]\n\
             Session: {}  Uptime: {}\n\
             State: {}  Route: {}\n\
             Local: {}\n\
             Remote: {}\n\
             ICE pair: {}\n\
             Channels: control={} video={}\n\
             RTT: {}  Bandwidth: receive={} available={}\n\
             Network received: {} packets / {}\n\
             Display: {}  Stream: {} @ {} fps\n\
             Codec: {}\n\
             FPS: receive={:.1} decode={} present={}\n\
             Frames: received={} presented={}\n\
             Drops: network={} stale={} presenter={} decoder={}\n\
             Packet errors: duplicate={} invalid={}\n\
             Latest remote encode: {:.1} ms",
            debug.session_id,
            format_duration(debug.started.elapsed()),
            debug.connection_state,
            debug.connection_path,
            debug.local_candidate,
            debug.remote_candidate,
            debug.selected_pair,
            debug.control_channel,
            debug.video_channel,
            rtt,
            receive_bandwidth,
            available_bandwidth,
            debug.packets_received,
            format_bytes(debug.bytes_received),
            debug.display,
            debug.stream,
            debug.target_fps,
            debug.codec,
            debug.received_fps,
            decode_fps,
            present_fps,
            debug.frames_received,
            debug.frames_presented,
            debug.incomplete_frames_dropped,
            debug.stale_packets_dropped,
            debug.presenter_frames_dropped,
            debug.decoded_frames_dropped,
            debug.duplicate_packets,
            debug.invalid_packets,
            debug.last_encode_ms,
        )
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        (seconds / 60) % 60,
        seconds % 60
    )
}

fn format_bytes(bytes: u64) -> String {
    const MIB: f64 = 1_048_576.0;
    const KIB: f64 = 1_024.0;
    if bytes >= 1_048_576 {
        format!("{:.2} MiB", bytes as f64 / MIB)
    } else if bytes >= 1_024 {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_diagnostics_include_the_essential_sections() {
        let debug = DebugInfo::new("session-123");
        debug.set_selected_pair("TURN relay", "relay -> host");
        debug.configure_stream("Display 1", 1920, 1080, 60, Codec::H264);
        let text = debug.render();
        assert!(text.contains("session-123"));
        assert!(text.contains("TURN relay"));
        assert!(text.contains("Stream: 1920x1080 @ 60 fps"));
        assert!(text.contains("Codec: H.264 / AVC"));
        assert!(text.contains("Bandwidth: receive=-- available=--"));
        assert!(text.contains("FPS:"));
        assert!(text.contains("Drops:"));
    }
}
