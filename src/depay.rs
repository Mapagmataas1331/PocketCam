//! RFC 6184 depay. Resets FU-A on the start bit so a lost fragment cannot
//! glue two NALUs together (that is the ghosting / smear).

const START: [u8; 4] = [0, 0, 0, 1];
const TYPE_MASK: u8 = 0x1F;
const FU_START: u8 = 0x80;
const FU_END: u8 = 0x40;

pub struct H264Depay {
    fu: Vec<u8>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    au: Vec<u8>,
    last_ts: Option<u32>,
    last_seq: Option<u16>,
    broken: bool,
    pub seq_gaps: u64,
    pub dropped_incomplete: u64,
}

impl Default for H264Depay {
    fn default() -> Self {
        Self {
            fu: Vec::with_capacity(64 * 1024),
            sps: Vec::new(),
            pps: Vec::new(),
            au: Vec::with_capacity(128 * 1024),
            last_ts: None,
            last_seq: None,
            broken: false,
            seq_gaps: 0,
            dropped_incomplete: 0,
        }
    }
}

impl H264Depay {
    /// Push one RTP packet. Returns zero, one, or two complete access units
    /// (previous timestamp close, then marker close).
    pub fn push(&mut self, seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Vec<(Vec<u8>, u32)> {
        let mut finished = Vec::new();
        let gap = self
            .last_seq
            .map(|prev| seq != prev.wrapping_add(1))
            .unwrap_or(false);
        if gap {
            self.seq_gaps += 1;
        }

        if self.last_ts.map(|t| t != ts).unwrap_or(false) {
            if gap {
                // Lost the tail of the previous frame.
                self.fu.clear();
                if !self.au.is_empty() {
                    self.au.clear();
                    self.dropped_incomplete += 1;
                }
                self.broken = false;
            } else if let Some(au) = self.take_au() {
                finished.push((au, self.last_ts.unwrap_or(0)));
            }
        }

        if gap {
            self.fu.clear();
            self.broken = true;
        }
        self.last_seq = Some(seq);
        self.last_ts = Some(ts);
        self.ingest(payload);
        if marker {
            if let Some(au) = self.take_au() {
                finished.push((au, ts));
            }
        }
        finished
    }

    fn ingest(&mut self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        let nalu_type = payload[0] & TYPE_MASK;
        match nalu_type {
            1..=23 => {
                if nalu_type == 9 || nalu_type == 12 {
                    return;
                }
                self.note_param(payload);
                self.au.extend_from_slice(&START);
                self.au.extend_from_slice(payload);
            }
            24 => self.ingest_stap_a(payload),
            28 => self.ingest_fu_a(payload),
            _ => {}
        }
    }

    fn ingest_stap_a(&mut self, payload: &[u8]) {
        let mut off = 1usize;
        while off + 2 <= payload.len() {
            let nalu_size = u16::from_be_bytes([payload[off], payload[off + 1]]) as usize;
            off += 2;
            if off + nalu_size > payload.len() {
                break;
            }
            let nalu = &payload[off..off + nalu_size];
            off += nalu_size;
            if nalu.is_empty() {
                continue;
            }
            let t = nalu[0] & TYPE_MASK;
            if t == 9 || t == 12 {
                continue;
            }
            self.note_param(nalu);
            self.au.extend_from_slice(&START);
            self.au.extend_from_slice(nalu);
        }
    }

    fn ingest_fu_a(&mut self, payload: &[u8]) {
        if payload.len() < 3 {
            return;
        }
        let start = payload[1] & FU_START != 0;
        let end = payload[1] & FU_END != 0;
        if start {
            self.fu.clear();
            let nal_hdr = (payload[0] & 0xE0) | (payload[1] & TYPE_MASK);
            self.fu.push(nal_hdr);
        } else if self.fu.is_empty() {
            return;
        }
        self.fu.extend_from_slice(&payload[2..]);
        if end {
            let nal = std::mem::take(&mut self.fu);
            if nal.is_empty() {
                return;
            }
            self.note_param(&nal);
            self.au.extend_from_slice(&START);
            self.au.extend_from_slice(&nal);
        }
    }

    fn note_param(&mut self, nalu: &[u8]) {
        if nalu.is_empty() {
            return;
        }
        match nalu[0] & TYPE_MASK {
            7 => self.sps = nalu.to_vec(),
            8 => self.pps = nalu.to_vec(),
            _ => {}
        }
    }

    fn take_au(&mut self) -> Option<Vec<u8>> {
        let incomplete_fu = !self.fu.is_empty();
        self.fu.clear();
        let broken = self.broken || incomplete_fu;
        self.broken = false;
        if self.au.is_empty() {
            return None;
        }
        let mut out = std::mem::take(&mut self.au);
        if broken {
            self.dropped_incomplete += 1;
            return None;
        }
        if has_slice_type(&out, 5) && !has_slice_type(&out, 7) && !self.sps.is_empty() {
            let mut with_params = Vec::with_capacity(self.sps.len() + self.pps.len() + out.len() + 8);
            with_params.extend_from_slice(&START);
            with_params.extend_from_slice(&self.sps);
            if !self.pps.is_empty() {
                with_params.extend_from_slice(&START);
                with_params.extend_from_slice(&self.pps);
            }
            with_params.extend_from_slice(&out);
            out = with_params;
        }
        Some(out)
    }
}

fn has_slice_type(annex_b: &[u8], nal_type: u8) -> bool {
    let mut i = 0;
    while i + 4 < annex_b.len() {
        if annex_b[i..i + 4] == START {
            if (annex_b[i + 4] & TYPE_MASK) == nal_type {
                return true;
            }
            i += 4;
        } else {
            i += 1;
        }
    }
    false
}

pub fn is_idr(annex_b: &[u8]) -> bool {
    has_slice_type(annex_b, 5)
}
