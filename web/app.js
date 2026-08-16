/* PocketCam phone page. Token from /connect/{token} or typed from the PC. */
(function () {
  const logEl = document.getElementById("log");
  const actionBtn = document.getElementById("action");
  const preview = document.getElementById("preview");
  const cameraSel = document.getElementById("camera");
  const resSel = document.getElementById("res");
  const fpsSel = document.getElementById("fps");
  const qualityOffer = document.getElementById("quality-offer");
  const qualityOfferYes = document.getElementById("quality-offer-yes");
  const qualityOfferNo = document.getElementById("quality-offer-no");
  const tokenInput = document.getElementById("token");
  const scanBtn = document.getElementById("scan-qr");
  const statusEl = document.getElementById("status");
  const bannerEl = document.getElementById("banner");
  const stageEl = document.getElementById("stage");
  const shellEl = document.querySelector(".shell");
  const unpauseBtn = document.getElementById("unpause-preview");
  const idleVeil = document.getElementById("idle-veil");
  if (preview) {
    preview.playsInline = true;
    preview.muted = true;
    try {
      preview.disablePictureInPicture = true;
    } catch (e) {}
  }

  function fitSafariChrome() {
    const vv = window.visualViewport;
    if (!shellEl) return;
    const h = vv ? vv.height : window.innerHeight;
    const t = vv ? vv.offsetTop : 0;
    shellEl.style.top = t + "px";
    shellEl.style.height = h + "px";
    shellEl.style.left = "0";
    shellEl.style.right = "auto";
    shellEl.style.bottom = "auto";
    shellEl.style.width = "100%";
  }
  fitSafariChrome();
  window.addEventListener("resize", fitSafariChrome);
  if (window.visualViewport) {
    window.visualViewport.addEventListener("resize", fitSafariChrome);
    window.visualViewport.addEventListener("scroll", fitSafariChrome);
  }

  function tokenFromLocation() {
    const m = location.pathname.match(/\/connect\/([^/]+)/);
    if (m) return decodeURIComponent(m[1]);
    return new URLSearchParams(location.search).get("t") || "";
  }

  function normalizeToken(s) {
    return String(s || "")
      .toUpperCase()
      .replace(/[^0-9A-Z]/g, "");
  }

  function prettyToken(s) {
    const t = normalizeToken(s);
    if (t.length > 4) return t.slice(0, 4) + " " + t.slice(4, 8);
    return t;
  }

  const DEBUG = /(?:^|[?&])debug=1(?:&|$)/.test(location.search);
  const logWrap = document.getElementById("log-wrap");
  if (logWrap && DEBUG) logWrap.hidden = false;
  tokenInput.value = prettyToken(tokenFromLocation());

  const RES_PRESETS = [
    { h: 480, w: 854, label: "480p" },
    { h: 720, w: 1280, label: "720p" },
    { h: 1080, w: 1920, label: "1080p" },
    { h: 1440, w: 2560, label: "1440p" },
    { h: 2160, w: 3840, label: "4K" },
  ];
  const FPS_PRESETS = [15, 24, 25, 30, 50, 60];
  const KBPS_30 = { 480: 2500, 720: 4000, 1080: 8000, 1440: 14000, 2160: 18000 };
  const KBPS_60 = { 480: 3500, 720: 6000, 1080: 12000, 1440: 20000, 2160: 28000 };

  function modeKbps(h, fps) {
    const a = KBPS_30[h] || 8000;
    const b = KBPS_60[h] || 12000;
    if (fps <= 30) return Math.round(a * (fps / 30));
    return Math.round(a + (b - a) * ((fps - 30) / 30));
  }

  function resLabel(h) {
    const r = RES_PRESETS.find(function (x) {
      return x.h === h;
    });
    return r ? r.label : h + "p";
  }

  function buildPipelineModes() {
    const out = [];
    RES_PRESETS.forEach(function (r) {
      FPS_PRESETS.forEach(function (fps) {
        const id = r.h + "p" + fps;
        let label = r.label + " " + fps + " fps";
        if (id === "1080p30") label += " — recommended";
        else if (r.h >= 2160 && fps >= 30) label += " — needs strong Wi-Fi";
        out.push({
          id: id,
          w: r.w,
          h: r.h,
          fps: fps,
          kbps: modeKbps(r.h, fps),
          label: label,
        });
      });
    });
    return out;
  }

  const PIPELINE_MODES = buildPipelineModes();

  function pipelineMode(id) {
    for (let i = 0; i < PIPELINE_MODES.length; i++) {
      if (PIPELINE_MODES[i].id === id) return PIPELINE_MODES[i];
    }
    return (
      PIPELINE_MODES.find(function (m) {
        return m.id === "1080p30";
      }) || PIPELINE_MODES[0]
    );
  }

  let sender = null;
  let activeStream = null;
  let ws = null;
  let pc = null;
  let wakeLock = null;
  let sessionActive = false;
  let starting = false;
  let availableModes = [];
  let modeMarks = {};
  let selectedMode = pipelineMode("1080p30");
  let offer60Dismissed = false;
  let qualityLock = null;
  let tuneSender = async function () {};
  let cameraBusy = false;
  let resumeOnVisible = false;
  let scanning = false;
  let scanStream = null;
  let scanRaf = 0;
  let jsQRLoader = null;
  let previewPaused = false;
  let idlePreviewTimer = 0;
  let idleVeilTimer = 0;
  const PREVIEW_IDLE_MS = 20 * 1000;
  const VEIL_IDLE_MS = 3 * 60 * 1000;
  const deviceFailed = {};
  const scanCanvas = document.createElement("canvas");
  const scanCtx = scanCanvas.getContext("2d", { willReadFrequently: true });

  function sleep(ms) {
    return new Promise(function (resolve) {
      setTimeout(resolve, ms);
    });
  }

  function tokenFromScanText(text) {
    const raw = String(text || "").trim();
    if (!raw) return "";
    if (/^https?:\/\//i.test(raw) || raw.indexOf("/connect/") !== -1) {
      try {
        const u = new URL(raw, location.origin);
        const m = u.pathname.match(/\/connect\/([^/]+)/);
        if (m) return normalizeToken(m[1]);
        const q = u.searchParams.get("t");
        if (q) return normalizeToken(q);
      } catch (e) {}
    }
    const n = normalizeToken(raw);
    if (n.length === 8) return n;
    return "";
  }

  function loadJsQR() {
    if (window.jsQR) return Promise.resolve(window.jsQR);
    if (jsQRLoader) return jsQRLoader;
    jsQRLoader = new Promise(function (resolve, reject) {
      const s = document.createElement("script");
      s.src = "/jsQR.js";
      s.onload = function () {
        if (window.jsQR) resolve(window.jsQR);
        else reject(new Error("jsQR missing"));
      };
      s.onerror = function () {
        reject(new Error("could not load QR scanner"));
      };
      document.head.appendChild(s);
    });
    return jsQRLoader;
  }

  async function stopScan() {
    scanning = false;
    if (scanRaf) {
      cancelAnimationFrame(scanRaf);
      scanRaf = 0;
    }
    if (scanStream) {
      scanStream.getTracks().forEach(function (t) {
        t.stop();
      });
      scanStream = null;
    }
    stageEl.classList.remove("scanning");
    scanBtn.textContent = "Scan QR on the PC";
    if (!sessionActive && !starting && !activeStream) {
      preview.srcObject = null;
      setStageHasVideo(false);
    } else if (activeStream) {
      preview.srcObject = activeStream;
    }
  }

  async function applyScannedToken(token) {
    tokenInput.value = prettyToken(token);
    try {
      history.replaceState(null, "", "/connect/" + token);
    } catch (e) {}
    await stopScan();
    await sleep(250);
    log("scanned QR");
    start();
  }

  async function startScan() {
    if (sessionActive || starting || scanning) return;
    resetIdleState();
    if (!window.isSecureContext) {
      setBanner("Open this page over HTTPS, then scan the QR on the PC.", true);
      return;
    }
    scanning = true;
    scanBtn.textContent = "Cancel scan";
    setStatus("connecting", "Scan QR");
    setBanner("Point the camera at the QR on the PC.");
    try {
      let detector = null;
      if (window.BarcodeDetector) {
        try {
          detector = new BarcodeDetector({ formats: ["qr_code"] });
        } catch (e) {
          detector = null;
        }
      }
      const decode = detector ? null : await loadJsQR();
      scanStream = await navigator.mediaDevices.getUserMedia({
        audio: false,
        video: { facingMode: { ideal: "environment" } },
      });
      preview.srcObject = scanStream;
      setStageHasVideo(true);
      stageEl.classList.add("scanning");
      await preview.play().catch(function () {});
      let skip = false;
      const tick = async function () {
        if (!scanning) return;
        skip = !skip;
        if (skip) {
          scanRaf = requestAnimationFrame(tick);
          return;
        }
        try {
          let text = "";
          if (detector && preview.readyState >= 2) {
            const codes = await detector.detect(preview);
            if (codes && codes.length) text = codes[0].rawValue || "";
          } else if (decode && preview.videoWidth) {
            const w = preview.videoWidth;
            const h = preview.videoHeight;
            const maxW = 480;
            const scale = Math.min(1, maxW / w);
            const dw = Math.max(2, Math.round(w * scale));
            const dh = Math.max(2, Math.round(h * scale));
            if (scanCanvas.width !== dw) scanCanvas.width = dw;
            if (scanCanvas.height !== dh) scanCanvas.height = dh;
            scanCtx.drawImage(preview, 0, 0, dw, dh);
            const img = scanCtx.getImageData(0, 0, dw, dh);
            const code = decode(img.data, dw, dh, { inversionAttempts: "dontInvert" });
            if (code && code.data) text = code.data;
          }
          const token = tokenFromScanText(text);
          if (token.length === 8 && scanning) {
            await applyScannedToken(token);
            return;
          }
        } catch (e) {}
        if (scanning) scanRaf = requestAnimationFrame(tick);
      };
      scanRaf = requestAnimationFrame(tick);
    } catch (e) {
      log("scan failed: " + e);
      await stopScan();
      setStatus("err", "Scan failed");
      setBanner("Could not open the camera to scan. Type the token instead.", true);
    }
  }

  let logLines = [];
  let wakeTimer = 0;

  function log(line) {
    const t = new Date().toISOString().slice(11, 23);
    logLines.push(t + " " + line);
    if (logLines.length > 80) logLines.splice(0, logLines.length - 80);
    logEl.textContent = logLines.join("\n") + "\n";
    if (DEBUG) console.log(line);
  }

  function setStatus(kind, text) {
    statusEl.textContent = text;
    statusEl.className = kind || "";
  }

  function setBanner(text, isErr) {
    bannerEl.textContent = text;
    bannerEl.className = "hint" + (isErr ? " err" : "");
  }

  function setStageHasVideo(on) {
    stageEl.classList.toggle("empty", !on);
  }

  function setActionLive(live) {
    sessionActive = live;
    document.documentElement.classList.toggle("is-live", live);
    actionBtn.disabled = false;
    actionBtn.textContent = live ? "End session" : "Start camera";
    actionBtn.classList.toggle("danger", live);
    scanBtn.hidden = live;
    if (live) {
      stopScan();
      bumpIdle();
    } else {
      resetIdleState();
    }
  }

  function clearIdleTimers() {
    if (idlePreviewTimer) {
      clearTimeout(idlePreviewTimer);
      idlePreviewTimer = 0;
    }
    if (idleVeilTimer) {
      clearTimeout(idleVeilTimer);
      idleVeilTimer = 0;
    }
  }

  function resetIdleState() {
    clearIdleTimers();
    previewPaused = false;
    if (stageEl) stageEl.classList.remove("is-paused");
    if (idleVeil) idleVeil.hidden = true;
  }

  function bumpIdle() {
    clearIdleTimers();
    if (!sessionActive || starting || scanning) return;
    if (!previewPaused) {
      idlePreviewTimer = setTimeout(pauseLocalPreview, PREVIEW_IDLE_MS);
    }
    if (idleVeil && idleVeil.hidden) {
      idleVeilTimer = setTimeout(showIdleVeil, VEIL_IDLE_MS);
    }
  }

  function pauseLocalPreview() {
    if (!sessionActive || scanning || previewPaused) return;
    previewPaused = true;
    stageEl.classList.add("is-paused");
    try {
      preview.pause();
    } catch (e) {}
    // Keep MediaStream tracks for WebRTC. Detach so the phone stops compositing.
    preview.srcObject = null;
  }

  async function resumeLocalPreview() {
    previewPaused = false;
    stageEl.classList.remove("is-paused");
    if (activeStream && (sessionActive || starting) && preview.srcObject !== activeStream) {
      preview.srcObject = activeStream;
      setStageHasVideo(true);
      await preview.play().catch(function () {});
    }
    bumpIdle();
  }

  function showIdleVeil() {
    if (!sessionActive || scanning) return;
    pauseLocalPreview();
    if (idleVeil) idleVeil.hidden = false;
  }

  function wakeFromIdle() {
    if (idleVeil) idleVeil.hidden = true;
    resumeLocalPreview();
  }

  function currentToken() {
    return normalizeToken(tokenInput.value);
  }

  tokenInput.addEventListener("input", function () {
    const start = tokenInput.selectionStart;
    const before = tokenInput.value;
    tokenInput.value = prettyToken(tokenInput.value);
    if (document.activeElement === tokenInput && start != null) {
      const delta = tokenInput.value.length - before.length;
      tokenInput.setSelectionRange(start + delta, start + delta);
    }
  });

  function keepOpenCopy() {
    if (!navigator.wakeLock || !navigator.wakeLock.request) {
      return "Keep the screen on. This browser cannot prevent sleep, and sleep stops the camera.";
    }
    return "Keep this page open. Switching apps or locking the phone stops the camera.";
  }

  function h264SenderCodecs() {
    if (!RTCRtpSender.getCapabilities) return [];
    const caps = RTCRtpSender.getCapabilities("video");
    if (!caps || !caps.codecs) return [];
    return caps.codecs.filter(function (c) {
      return /h264/i.test(c.mimeType);
    });
  }

  function preferH264Sdp(sdp) {
    const lines = sdp.split("\r\n").join("\n").split("\n");
    const mIndex = lines.findIndex((l) => l.startsWith("m=video"));
    if (mIndex < 0) return sdp;
    const pts = [];
    for (const line of lines) {
      const m = line.match(/^a=rtpmap:(\d+) H264\/90000/i);
      if (m) pts.push(m[1]);
    }
    if (!pts.length) return sdp;
    const parts = lines[mIndex].trim().split(" ");
    const head = parts.slice(0, 3);
    const payloads = parts.slice(3);
    const rest = payloads.filter((p) => pts.indexOf(p) < 0);
    lines[mIndex] = head.concat(pts, rest).join(" ");
    return lines.join("\r\n");
  }

  function stripGcc(sdp) {
    return sdp
      .split(/\r?\n/)
      .filter(function (l) {
        if (/goog-remb/i.test(l)) return false;
        if (/transport-cc/i.test(l)) return false;
        if (/transport-wide-cc/i.test(l)) return false;
        if (/draft-holmer-rmcat-transport-wide-cc/i.test(l)) return false;
        return true;
      })
      .join("\r\n");
  }

  function preferH264Transceiver(peer, sdr) {
    if (!RTCRtpSender.getCapabilities || !peer.getTransceivers) return;
    const t = peer.getTransceivers().find((x) => x.sender === sdr);
    if (!t || !t.setCodecPreferences) return;
    const caps = RTCRtpSender.getCapabilities("video");
    if (!caps || !caps.codecs) return;
    const h264 = caps.codecs.filter((c) => /h264/i.test(c.mimeType));
    const rtx = caps.codecs.filter((c) => /rtx/i.test(c.mimeType));
    const rest = caps.codecs.filter(
      (c) => !/h264/i.test(c.mimeType) && !/rtx/i.test(c.mimeType)
    );
    if (h264.length) {
      t.setCodecPreferences(h264.concat(rtx, rest));
      log("setCodecPreferences: " + h264.length + " H.264 codec(s)");
    } else {
      log("no H.264 in RTCRtpSender.getCapabilities");
    }
  }

  async function holdWake() {
    if (!sessionActive && !starting) return false;
    if (document.visibilityState !== "visible") return false;
    if (!navigator.wakeLock || !navigator.wakeLock.request) return false;
    if (wakeLock && !wakeLock.released) return true;
    try {
      const lock = await navigator.wakeLock.request("screen");
      wakeLock = lock;
      lock.addEventListener("release", function () {
        if (wakeLock === lock) wakeLock = null;
        if (sessionActive && document.visibilityState === "visible") {
          holdWake();
        }
      });
      return true;
    } catch (e) {
      log("wake lock: " + e);
      return false;
    }
  }

  function startWakeWatch() {
    if (wakeTimer) return;
    wakeTimer = setInterval(function () {
      if (sessionActive && document.visibilityState === "visible") holdWake();
    }, 30000);
  }

  function releaseWake() {
    if (wakeTimer) {
      clearInterval(wakeTimer);
      wakeTimer = 0;
    }
    const lock = wakeLock;
    wakeLock = null;
    if (lock && lock.release) {
      lock.release().catch(function () {});
    }
  }

  document.addEventListener("visibilitychange", async function () {
    if (document.visibilityState !== "visible") return;
    if (sessionActive && navigator.wakeLock) {
      try {
        wakeLock = await navigator.wakeLock.request("screen");
      } catch (e) {}
    }
    if (sessionActive) bumpIdle();
    if (resumeOnVisible && !sessionActive && !starting && !scanning && currentToken()) {
      log("page visible — reconnecting with the same token");
      start();
    }
  });

  async function listCameras() {
    if (!navigator.mediaDevices || !navigator.mediaDevices.enumerateDevices) return [];
    const devices = await navigator.mediaDevices.enumerateDevices();
    const cams = devices.filter((d) => d.kind === "videoinput");
    const prev = cameraSel.value;
    cameraSel.innerHTML = "";
    const def = document.createElement("option");
    def.value = "";
    def.textContent = "Default (back camera)";
    cameraSel.appendChild(def);
    cams.forEach((d, i) => {
      const opt = document.createElement("option");
      opt.value = d.deviceId;
      opt.textContent = d.label || "Camera " + (i + 1);
      cameraSel.appendChild(opt);
    });
    if (prev && cams.some((d) => d.deviceId === prev)) cameraSel.value = prev;
    return cams.map((d, i) => ({
      id: d.deviceId,
      label: d.label || "Camera " + (i + 1),
    }));
  }

  function capMax(range, fallback) {
    if (!range) return fallback;
    if (typeof range.max === "number") return range.max;
    if (typeof range === "number") return range;
    return fallback;
  }

  function facingOf(caps, settings) {
    const fromSettings = settings && settings.facingMode;
    if (fromSettings) return fromSettings;
    const f = caps && caps.facingMode;
    if (Array.isArray(f) && f.length) return f[0];
    if (typeof f === "string") return f;
    return "";
  }

  function markFailed(deviceId, modeId) {
    if (!deviceId || !modeId) return;
    if (!deviceFailed[deviceId]) deviceFailed[deviceId] = {};
    deviceFailed[deviceId][modeId] = true;
  }

  function isFailed(deviceId, modeId) {
    return !!(deviceId && deviceFailed[deviceId] && deviceFailed[deviceId][modeId]);
  }

  function isUserFacing(caps, settings, track) {
    if (facingOf(caps, settings) === "user") return true;
    const label = ((track && track.label) || "").toLowerCase();
    return /front|face|user/.test(label);
  }

  function longSlack(need) {
    return Math.max(80, Math.round(need * 0.12));
  }

  function modeAllowedByLock(mode) {
    if (!mode || !qualityLock) return true;
    if (mode.w !== qualityLock.w || mode.h !== qualityLock.h) return false;
    if (qualityLock.maxFps != null && mode.fps > qualityLock.maxFps) return false;
    return true;
  }

  function deviceCanDo(caps, settings, mode, deviceId, track) {
    if (isFailed(deviceId, mode.id)) return "no";
    if (isUserFacing(caps, settings, track) && Math.max(mode.w, mode.h) > 1920) return "no";
    const maxW = capMax(caps.width, 0);
    const maxH = capMax(caps.height, 0);
    const maxF = capMax(caps.frameRate, 0);
    const hasCaps = maxW > 0 && maxH > 0;
    if (hasCaps) {
      const long = Math.max(maxW, maxH);
      const short = Math.min(maxW, maxH) || long;
      const needL = Math.max(mode.w, mode.h);
      const needS = Math.min(mode.w, mode.h);
      if (long + longSlack(needL) < needL) return "no";
      if (short + longSlack(needS) < needS) return "no";
      if (maxF > 0 && maxF + 0.01 < mode.fps) return "no";
      return "yes";
    }
    const gotW = (settings && settings.width) || 0;
    const gotH = (settings && settings.height) || 0;
    const gotF = (settings && settings.frameRate) || 0;
    if (!gotW || !gotH) return "unknown";
    const gotLong = Math.max(gotW, gotH);
    const gotShort = Math.min(gotW, gotH) || gotLong;
    const needL = Math.max(mode.w, mode.h);
    const needS = Math.min(mode.w, mode.h);
    if (needL <= gotLong + longSlack(needL) && needS <= gotShort + longSlack(needS)) {
      if (gotF > 0 && mode.fps > gotF + 1.5) return "unknown";
      return "yes";
    }
    if (needL > gotLong * 1.35 + 48) return "unknown";
    return "unknown";
  }

  function modesFromTrack(track) {
    const caps =
      track && track.getCapabilities ? track.getCapabilities() : {};
    const settings = track && track.getSettings ? track.getSettings() : {};
    const deviceId = settings.deviceId || cameraSel.value || "";
    const marks = {};
    const yes = [];
    PIPELINE_MODES.forEach(function (m) {
      const mark = deviceCanDo(caps, settings, m, deviceId, track);
      marks[m.id] = mark;
      if (mark === "yes") yes.push(m);
    });
    modeMarks = marks;
    return yes;
  }

  function markColor(mark) {
    if (mark === "yes") return "#72a87e";
    if (mark === "no") return "#c47676";
    return "#c4a85c";
  }

  function markOf(mode) {
    if (!mode) return "unknown";
    if (!modeAllowedByLock(mode)) return "no";
    return modeMarks[mode.id] || "unknown";
  }

  function resMark(h) {
    let acc = "no";
    for (let i = 0; i < PIPELINE_MODES.length; i++) {
      const m = PIPELINE_MODES[i];
      if (m.h !== h) continue;
      const a = markOf(m);
      if (a === "yes") return "yes";
      if (a === "unknown") acc = "unknown";
    }
    return acc;
  }

  function fpsMark(h, fps) {
    return markOf(
      PIPELINE_MODES.find(function (m) {
        return m.h === h && m.fps === fps;
      })
    );
  }

  function modeScore(m) {
    return m.w * m.h * m.fps;
  }

  function pickModeForCamera(wanted, supported) {
    if (!supported.length) return wanted || pipelineMode("1080p30");
    if (wanted && supported.some(function (m) { return m.id === wanted.id; })) {
      return supported.find(function (m) { return m.id === wanted.id; });
    }
    const want = wanted ? modeScore(wanted) : modeScore(pipelineMode("1080p30"));
    const below = supported.filter(function (m) {
      return modeScore(m) <= want;
    });
    const pool = (below.length ? below : supported).slice().sort(function (a, b) {
      return modeScore(b) - modeScore(a);
    });
    return (
      pool.find(function (m) { return m.id === "1080p30"; }) || pool[0]
    );
  }

  function learnFromCapture(track, requested) {
    if (!track || !requested) return;
    const settings = track.getSettings ? track.getSettings() : {};
    const deviceId = settings.deviceId || cameraSel.value || "";
    const gotLong = Math.max(settings.width || 0, settings.height || 0);
    const wantLong = Math.max(requested.w, requested.h);
    const gotFps = settings.frameRate || 0;
    if (gotLong > 0 && wantLong > gotLong * 1.2) {
      PIPELINE_MODES.forEach(function (m) {
        if (Math.max(m.w, m.h) > gotLong * 1.25 + 48) markFailed(deviceId, m.id);
      });
      log(
        "this camera delivered " +
          gotLong +
          " px for " +
          requested.id +
          " — disabling larger modes"
      );
    }
    if (requested.fps >= 50 && gotFps > 0 && gotFps < 45) {
      markFailed(deviceId, requested.id);
      PIPELINE_MODES.forEach(function (m) {
        if (m.fps >= 50 && Math.max(m.w, m.h) >= wantLong) markFailed(deviceId, m.id);
      });
      log("this camera did not hold " + requested.fps + " fps — disabling matching 60 fps modes");
    }
  }

  function fillQualitySelect() {
    if (!resSel || !fpsSel) return;
    const h = (selectedMode && selectedMode.h) || 1080;
    const fps = (selectedMode && selectedMode.fps) || 30;
    resSel.innerHTML = "";
    RES_PRESETS.forEach(function (r) {
      const opt = document.createElement("option");
      opt.value = String(r.h);
      opt.disabled = false;
      const extra = r.h === 1080 ? " — recommended" : "";
      opt.textContent = r.label + extra;
      opt.style.color = markColor(resMark(r.h));
      resSel.appendChild(opt);
    });
    fpsSel.innerHTML = "";
    FPS_PRESETS.forEach(function (f) {
      const opt = document.createElement("option");
      opt.value = String(f);
      opt.disabled = false;
      opt.textContent = String(f);
      opt.style.color = markColor(fpsMark(h, f));
      fpsSel.appendChild(opt);
    });
    resSel.value = String(h);
    fpsSel.value = String(fps);
    resSel.style.color = markColor(resMark(h));
    fpsSel.style.color = markColor(fpsMark(h, fps));
    resSel.disabled = false;
    fpsSel.disabled = false;
    refreshQualityOffer();
  }

  function modeFromSelects() {
    const h = parseInt(resSel && resSel.value, 10) || 1080;
    const fps = parseInt(fpsSel && fpsSel.value, 10) || 30;
    return pipelineMode(h + "p" + fps);
  }

  function sixtySibling(mode) {
    if (!mode || mode.fps >= 50) return null;
    for (let i = 0; i < PIPELINE_MODES.length; i++) {
      const m = PIPELINE_MODES[i];
      if (m.w === mode.w && m.h === mode.h && m.fps === 60) return m;
    }
    return null;
  }

  function refreshQualityOffer() {
    const sixty = sixtySibling(selectedMode);
    const can60 = !!(sixty && markOf(sixty) === "yes");
    const showing30 = selectedMode && selectedMode.fps <= 30;
    const show = can60 && showing30 && !offer60Dismissed && sessionActive;
    qualityOffer.hidden = !show;
  }

  function sendQualities() {
    if (!ws || ws.readyState !== 1) return;
    ws.send(
      JSON.stringify({
        type: "qualities",
        qualities: PIPELINE_MODES.map(function (m) {
          const a = markOf(m);
          return {
            id: m.id,
            label: m.label,
            available: a === "yes" ? true : a === "no" ? false : null,
          };
        }),
        qualityId: selectedMode ? selectedMode.id : "",
      })
    );
    if (selectedMode) {
      ws.send(
        JSON.stringify({
          type: "quality",
          qualityId: selectedMode.id,
          label: selectedMode.label,
        })
      );
    }
  }

  function sendCameras(cams, selectedId, selectedLabel) {
    if (!ws || ws.readyState !== 1) return;
    ws.send(JSON.stringify({ type: "cameras", devices: cams }));
    if (selectedId) {
      ws.send(
        JSON.stringify({
          type: "camera",
          deviceId: selectedId,
          label: selectedLabel || "",
        })
      );
    }
  }

  async function getCameraStream(deviceId, mode, opts) {
    const m = mode || selectedMode || pipelineMode("1080p30");
    const skipExact = opts && opts.skipExact;
    const base = deviceId
      ? { deviceId: { exact: deviceId } }
      : { facingMode: { ideal: "environment" } };
    const tries = [];
    if (!skipExact) {
      tries.push({
        audio: false,
        video: Object.assign({}, base, {
          width: { exact: m.w },
          height: { exact: m.h },
          frameRate: { exact: m.fps },
        }),
      });
    }
    tries.push({
      audio: false,
      video: Object.assign({}, base, {
        width: { ideal: m.w },
        height: { ideal: m.h },
        frameRate: { ideal: m.fps, max: m.fps },
      }),
    });
    tries.push({
      audio: false,
      video: Object.assign({}, base, {
        width: { ideal: m.w },
        height: { ideal: m.h },
        frameRate: { ideal: m.fps },
      }),
    });
    tries.push({
      audio: false,
      video: Object.assign({}, base, {
        width: { ideal: m.h },
        height: { ideal: m.w },
        frameRate: { ideal: m.fps, max: m.fps },
      }),
    });
    if (Math.max(m.w, m.h) > 1920) {
      tries.push({
        audio: false,
        video: Object.assign({}, base, {
          width: { ideal: 1920 },
          height: { ideal: 1080 },
          frameRate: { ideal: 30, max: 30 },
        }),
      });
    }
    if (Math.max(m.w, m.h) > 1280) {
      tries.push({
        audio: false,
        video: Object.assign({}, base, {
          width: { ideal: 1280 },
          height: { ideal: 720 },
          frameRate: { ideal: 30 },
        }),
      });
    }
    tries.push({ audio: false, video: base });
    let lastErr;
    let best = null;
    let bestLong = 0;
    const want = Math.max(m.w, m.h);
    for (let i = 0; i < tries.length; i++) {
      try {
        const stream = await navigator.mediaDevices.getUserMedia(tries[i]);
        const t = stream.getVideoTracks()[0];
        const s = t && t.getSettings ? t.getSettings() : {};
        const got = Math.max(s.width || 0, s.height || 0);
        log(
          "getUserMedia try " +
            (i + 1) +
            " got " +
            (s.width || "?") +
            "x" +
            (s.height || "?")
        );
        if (got >= want * 0.85) {
          if (best) best.getTracks().forEach(function (tr) { tr.stop(); });
          return stream;
        }
        if (got > bestLong) {
          if (best) best.getTracks().forEach(function (tr) { tr.stop(); });
          best = stream;
          bestLong = got;
        } else {
          stream.getTracks().forEach(function (tr) { tr.stop(); });
        }
      } catch (e) {
        lastErr = e;
        log("getUserMedia try " + (i + 1) + " failed: " + e);
      }
    }
    if (best) return best;
    throw lastErr || new Error("getUserMedia failed");
  }

  function hintTrack(track) {
    try {
      if (track && "contentHint" in track) {
        track.contentHint =
          selectedMode && selectedMode.fps >= 48 ? "motion" : "detail";
      }
    } catch (e) {}
  }

  async function releaseCamera() {
    const old = activeStream;
    activeStream = null;
    if (sender) {
      try {
        await sender.replaceTrack(null);
      } catch (e) {}
    }
    if (old) {
      old.getTracks().forEach(function (t) {
        t.stop();
      });
    }
    preview.srcObject = null;
  }

  async function adoptTrack(stream) {
    const track = stream.getVideoTracks()[0];
    hintTrack(track);
    if (sender) {
      await sender.replaceTrack(track);
    }
    if (activeStream && activeStream !== stream) {
      activeStream.getTracks().forEach(function (t) {
        t.stop();
      });
    }
    activeStream = stream;
    if (!previewPaused) {
      preview.srcObject = stream;
      setStageHasVideo(true);
      await preview.play().catch(function () {});
    }
    return track;
  }

  function refreshModesFromTrack(track, requested) {
    if (requested) learnFromCapture(track, requested);
    availableModes = modesFromTrack(track);
    const caps = track.getCapabilities ? track.getCapabilities() : {};
    const settings = track.getSettings ? track.getSettings() : {};
    log(
      "caps max " +
        capMax(caps.width, 0) +
        "x" +
        capMax(caps.height, 0) +
        " @" +
        capMax(caps.frameRate, 0) +
        " facing=" +
        (facingOf(caps, settings) || "?")
    );
    const prevId = selectedMode && selectedMode.id;
    if (
      selectedMode &&
      availableModes.length &&
      !availableModes.some(function (m) {
        return m.id === selectedMode.id;
      })
    ) {
      selectedMode = pickModeForCamera(selectedMode, availableModes);
    }
    fillQualitySelect();
    sendQualities();
    log(
      "track: " +
        (settings.width || "?") +
        "x" +
        (settings.height || "?") +
        " @" +
        (settings.frameRate || "?") +
        " fps · mode " +
        (selectedMode && selectedMode.id) +
        " · " +
        availableModes.length +
        "/" +
        PIPELINE_MODES.length +
        " available"
    );
    return prevId !== (selectedMode && selectedMode.id);
  }

  async function applyQualityMode(mode, opts) {
    if (!mode) return;
    if (!modeAllowedByLock(mode)) {
      log("quality " + mode.id + " blocked by capture lock");
      fillQualitySelect();
      return;
    }
    selectedMode = mode;
    fillQualitySelect();
    refreshQualityOffer();
    sendQualities();
    const track = activeStream && activeStream.getVideoTracks()[0];
    if (!track) return;
    hintTrack(track);
    const constraints = {
      width: { ideal: mode.w },
      height: { ideal: mode.h },
      frameRate: { ideal: mode.fps },
    };
    try {
      await track.applyConstraints(constraints);
    } catch (e) {
      log("applyConstraints failed: " + e);
      await releaseCamera();
      await sleep(200);
      const stream = await getCameraStream(cameraSel.value, mode, { skipExact: true });
      await adoptTrack(stream);
    }
    const live = activeStream && activeStream.getVideoTracks()[0];
    if (live) {
      learnFromCapture(live, mode);
      const fps = live.getSettings && live.getSettings().frameRate;
      if (fps && fps < 5) {
        log("capture is " + fps + " fps — dropping to 720p30");
        markFailed(live.getSettings().deviceId || cameraSel.value, mode.id);
        const fallback = pickModeForCamera(pipelineMode("720p30"), modesFromTrack(live));
        if (fallback.id !== mode.id) {
          await applyQualityMode(fallback, { note: "This camera could not hold that mode." });
          return;
        }
      }
      refreshModesFromTrack(live, mode);
    }
    await tuneSender();
    if (opts && opts.note) {
      setBanner(opts.note);
    } else if (mode.id.indexOf("2160") === 0) {
      setBanner("4K is on. Use strong Wi-Fi; drop back if it stutters.");
    } else if (sessionActive) {
      setBanner(keepOpenCopy());
    }
  }

  async function switchCamera(deviceId) {
    if (!sender || cameraBusy) return;
    cameraBusy = true;
    offer60Dismissed = false;
    try {
      await releaseCamera();
      await sleep(250);
      let mode = selectedMode || pipelineMode("1080p30");
      let stream;
      try {
        stream = await getCameraStream(deviceId, mode, { skipExact: true });
      } catch (e) {
        log("switch at " + mode.id + " failed: " + e);
        if (qualityLock) throw e;
        mode = pickModeForCamera(pipelineMode("1080p30"), availableModes);
        stream = await getCameraStream(deviceId, mode, { skipExact: true });
      }
      const track = await adoptTrack(stream);
      if (track.getSettings && track.getSettings().deviceId) {
        cameraSel.value = track.getSettings().deviceId;
      }
      log("switched camera: " + (track.label || deviceId));
      sendCameras(await listCameras(), cameraSel.value, track.label);
      refreshModesFromTrack(track, mode);
      await sleep(350);
      const live = activeStream && activeStream.getVideoTracks()[0];
      if (live) refreshModesFromTrack(live, selectedMode);
      await tuneSender();
    } finally {
      cameraBusy = false;
    }
  }

  async function waitGathering(peer) {
    if (peer.iceGatheringState === "complete") return;
    await Promise.race([
      new Promise((resolve) => {
        function onChange() {
          if (peer.iceGatheringState === "complete") {
            peer.removeEventListener("icegatheringstatechange", onChange);
            resolve();
          }
        }
        peer.addEventListener("icegatheringstatechange", onChange);
      }),
      new Promise((resolve) => setTimeout(resolve, 2500)),
    ]);
  }

  function waitHello(socket, timeoutMs) {
    return new Promise(function (resolve, reject) {
      const timer = setTimeout(function () {
        cleanup();
        reject(new Error("PC did not accept the session in time"));
      }, timeoutMs);
      function cleanup() {
        clearTimeout(timer);
        socket.removeEventListener("message", onMsg);
        socket.removeEventListener("close", onClose);
      }
      function onClose() {
        cleanup();
        reject(new Error("Disconnected before the PC accepted the token"));
      }
      function onMsg(ev) {
        let msg;
        try {
          msg = JSON.parse(ev.data);
        } catch (e) {
          return;
        }
        if (msg.type === "hello-ok") {
          cleanup();
          resolve(msg);
        } else if (msg.type === "error") {
          cleanup();
          const err = new Error(msg.message || "session rejected");
          err.code = msg.code || "";
          reject(err);
        }
      }
      socket.addEventListener("message", onMsg);
      socket.addEventListener("close", onClose);
    });
  }

  function attachSignalHandlers(socket, peer, cams, settings, track) {
    socket.onmessage = async function (ev) {
      let msg;
      try {
        msg = JSON.parse(ev.data);
      } catch (e) {
        return;
      }
      if (msg.type === "hello-ok") {
        sendCameras(cams, settings.deviceId, track.label);
        sendQualities();
      } else if (msg.type === "answer") {
        await peer.setRemoteDescription({ type: "answer", sdp: msg.sdp });
        log("remote answer set");
        await tuneSender();
        setStatus("live", "Live");
        setBanner(keepOpenCopy());
        refreshQualityOffer();
      } else if (msg.type === "select-camera" && msg.deviceId !== undefined) {
        cameraSel.value = msg.deviceId;
        try {
          await switchCamera(msg.deviceId);
        } catch (e) {
          log("pc camera select failed: " + e);
        }
      } else if (msg.type === "capture-lock") {
        if (!msg.width || !msg.height) {
          qualityLock = null;
        } else {
          qualityLock = {
            w: msg.width,
            h: msg.height,
            maxFps: typeof msg.maxFps === "number" ? msg.maxFps : null,
          };
        }
        fillQualitySelect();
        sendQualities();
      } else if (msg.type === "select-quality" && msg.qualityId) {
        let mode = null;
        for (let i = 0; i < PIPELINE_MODES.length; i++) {
          if (PIPELINE_MODES[i].id === msg.qualityId) {
            mode = PIPELINE_MODES[i];
            break;
          }
        }
        if (!mode || !modeAllowedByLock(mode)) {
          log("pc asked for locked " + msg.qualityId);
          sendQualities();
          return;
        }
        offer60Dismissed = true;
        qualityOffer.hidden = true;
        try {
          await applyQualityMode(mode);
        } catch (e) {
          log("pc quality select failed: " + e);
        }
      } else if (msg.type === "bye") {
        log("pc ended session: " + (msg.reason || ""));
        resumeOnVisible = false;
        endSession(
          (msg.message || "PC started a new session.") + " Tap Scan QR on the PC, or type the new token.",
          true,
          true
        );
      } else if (msg.type === "error") {
        log("pc error: " + msg.message);
        if (msg.code === "unknown-token" || msg.code === "token-used") {
          resumeOnVisible = false;
          endSession(
            (msg.message || "That token is not valid.") + " Tap Scan QR on the PC.",
            true,
            true
          );
          tokenInput.focus();
        } else {
          setStatus("err", "Error");
          setBanner(msg.message || "Something went wrong", true);
        }
      }
    };
    socket.onclose = function () {
      if (sessionActive || starting) {
        log("signaling closed");
        endSession("Disconnected. Reopen this page within 5 minutes — same token.", true, true);
      }
    };
  }

  async function start() {
    if (scanning) return;
    if (starting || sessionActive) return;
    const token = currentToken();
    if (!token) {
      setStatus("err", "Token needed");
      setBanner("Type the token, or tap Scan QR on the PC.", true);
      tokenInput.focus();
      return;
    }
    if (!window.isSecureContext) {
      setStatus("err", "Not HTTPS");
      setBanner("Open this page over HTTPS (scan the QR from the PC).", true);
      return;
    }
    if (!h264SenderCodecs().length) {
      setStatus("err", "No H.264");
      setBanner(
        "This browser cannot encode H.264. Use Safari on iPhone or Chrome on Android.",
        true
      );
      return;
    }

    starting = true;
    offer60Dismissed = false;
    setActionLive(true);
    setStatus("connecting", "Starting");
    setBanner("Allow camera access, then keep this page open.");

    try {
      await holdWake();
      startWakeWatch();
      if (!activeStream) {
        const stream = await getCameraStream(cameraSel.value, selectedMode);
        await adoptTrack(stream);
      }
      const track = activeStream.getVideoTracks()[0];
      const settings = track.getSettings ? track.getSettings() : {};
      const cams = await listCameras();
      if (settings.deviceId) cameraSel.value = settings.deviceId;
      const changed = refreshModesFromTrack(track, selectedMode);
      if (changed) {
        await applyQualityMode(selectedMode);
      }

      const proto = location.protocol === "https:" ? "wss:" : "ws:";
      ws = new WebSocket(proto + "//" + location.host + "/ws");
      await new Promise(function (resolve, reject) {
        ws.onopen = resolve;
        ws.onerror = function () {
          reject(new Error("websocket failed"));
        };
      });
      log("signaling open");
      const helloWait = waitHello(ws, 8000);
      ws.send(JSON.stringify({ type: "hello", token: token }));
      const hello = await helloWait;
      log("session accepted");
      resumeOnVisible = true;
      try {
        history.replaceState(null, "", "/connect/" + token);
      } catch (e) {}

      var iceServers = [];
      if (hello && hello.stun !== false) {
        iceServers.push({ urls: hello.stunUrl || "stun:stun.l.google.com:19302" });
      }
      pc = new RTCPeerConnection({ iceServers: iceServers });
      pc.oniceconnectionstatechange = function () {
        log("ice: " + pc.iceConnectionState);
        if (pc.iceConnectionState === "connected" || pc.iceConnectionState === "completed") {
          setStatus("live", "Live");
        } else if (pc.iceConnectionState === "disconnected") {
          setStatus("connecting", "Reconnecting");
        } else if (pc.iceConnectionState === "failed") {
          if (sessionActive) {
            endSession(
              "Link failed. Same Wi-Fi as the PC? Tap Start camera to retry.",
              true,
              true
            );
          }
        }
      };
      pc.onconnectionstatechange = function () {
        log("pc: " + pc.connectionState);
        if (pc.connectionState === "failed" && sessionActive) {
          endSession(
            "Link failed. Same Wi-Fi as the PC? Tap Start camera to retry.",
            true,
            true
          );
        }
      };

      sender = pc.addTrack(track, activeStream);
      preferH264Transceiver(pc, sender);
      hintTrack(track);
      tuneSender = async function () {
        try {
          const p = sender.getParameters();
          p.degradationPreference = "maintain-framerate";
          if (!p.encodings || !p.encodings.length) p.encodings = [{}];
          const kbps = (selectedMode && selectedMode.kbps) || 8000;
          const fps = (selectedMode && selectedMode.fps) || 30;
          p.encodings[0].maxBitrate = kbps * 1000;
          p.encodings[0].maxFramerate = fps;
          if ("minBitrate" in p.encodings[0]) {
            try {
              p.encodings[0].minBitrate = Math.round(kbps * 0.5) * 1000;
            } catch (e) {}
          }
          if ("priority" in p.encodings[0]) p.encodings[0].priority = "high";
          if ("networkPriority" in p.encodings[0]) p.encodings[0].networkPriority = "high";
          await sender.setParameters(p);
        } catch (e) {
          log("setParameters: " + e);
        }
      };
      await tuneSender();
      attachSignalHandlers(ws, pc, cams, settings, track);
      sendCameras(cams, settings.deviceId, track.label);
      sendQualities();

      let offer = await pc.createOffer({
        offerToReceiveAudio: false,
        offerToReceiveVideo: false,
      });
      offer = { type: offer.type, sdp: stripGcc(preferH264Sdp(offer.sdp)) };
      if (!/H264\/90000/i.test(offer.sdp)) {
        throw new Error(
          "This browser did not offer H.264. Use Safari on iPhone or Chrome on Android."
        );
      }
      await pc.setLocalDescription(offer);
      await waitGathering(pc);
      const local = pc.localDescription;
      ws.send(
        JSON.stringify({ type: "offer", sdp: stripGcc(preferH264Sdp(local.sdp)) })
      );
      log("offer sent");
      setStatus("connecting", "Connecting");
      starting = false;
      bumpIdle();
    } catch (e) {
      log("failed: " + e);
      starting = false;
      const tokenErr = e && (e.code === "unknown-token" || e.code === "token-used");
      endSession(e.message || String(e), true, false);
      if (tokenErr) tokenInput.focus();
    }
  }

  function stopTracks() {
    sender = null;
    if (activeStream) {
      activeStream.getTracks().forEach(function (t) {
        t.stop();
      });
      activeStream = null;
    }
    preview.srcObject = null;
    setStageHasVideo(false);
  }

  function endSession(message, asError, keepCamera) {
    const was = sessionActive || starting;
    starting = false;
    sessionActive = false;
    try {
      if (ws) {
        ws.onclose = null;
        ws.onmessage = null;
        if (ws.readyState === 1) ws.close();
      }
    } catch (e) {}
    ws = null;
    try {
      if (pc) pc.close();
    } catch (e) {}
    pc = null;
    sender = null;
    if (!keepCamera) stopTracks();
    if (resSel) resSel.disabled = false;
    if (fpsSel) fpsSel.disabled = false;
    qualityOffer.hidden = true;
    qualityLock = null;
    releaseWake();
    setActionLive(false);
    if (was || message) {
      setStatus(asError ? "err" : "", asError ? "Disconnected" : "Ready");
      setBanner(
        message || "Session ended. Type the token from the PC to start again.",
        !!asError
      );
    }
  }

  if (unpauseBtn) {
    unpauseBtn.addEventListener("click", function (ev) {
      ev.preventDefault();
      ev.stopPropagation();
      resumeLocalPreview();
    });
  }
  if (idleVeil) {
    idleVeil.addEventListener("click", function () {
      wakeFromIdle();
    });
  }
  document.addEventListener(
    "pointerdown",
    function (ev) {
      const t = ev.target;
      if (!t || !t.closest) return;
      if (t.closest("#idle-veil") || t.closest("#unpause-preview") || t.closest("#preview-paused")) {
        return;
      }
      bumpIdle();
    },
    { passive: true }
  );

  scanBtn.addEventListener("click", function () {
    if (sessionActive || starting) return;
    if (scanning) stopScan();
    else startScan();
  });
  actionBtn.addEventListener("click", function () {
    if (sessionActive || starting) {
      resumeOnVisible = false;
      endSession("Session ended. Reopen within 5 minutes with the same token, or tap New session on the PC.", false);
    } else {
      start();
    }
  });
  tokenInput.addEventListener("keydown", function (ev) {
    if (ev.key === "Enter" && !sessionActive && !starting) start();
  });
  cameraSel.addEventListener("change", async function (ev) {
    try {
      await switchCamera(cameraSel.value);
      if (ev.isTrusted) await resumeLocalPreview();
    } catch (e) {
      log("switch camera failed: " + e);
    }
  });
  async function onQualitySelects(ev) {
    const mode = modeFromSelects();
    if (!modeAllowedByLock(mode)) {
      fillQualitySelect();
      setBanner(
        qualityLock && qualityLock.maxFps != null
          ? "Recording: size is locked. You can only drop fps."
          : "Virtual camera is on: size is locked.",
        true
      );
      return;
    }
    offer60Dismissed = true;
    qualityOffer.hidden = true;
    try {
      await applyQualityMode(mode);
      if (ev && ev.isTrusted) await resumeLocalPreview();
    } catch (e) {
      log("quality change failed: " + e);
      setBanner("Could not switch quality: " + e, true);
    }
  }
  if (resSel) resSel.addEventListener("change", onQualitySelects);
  if (fpsSel) fpsSel.addEventListener("change", onQualitySelects);
  qualityOfferYes.addEventListener("click", async function () {
    offer60Dismissed = true;
    qualityOffer.hidden = true;
    try {
      const sixty =
        sixtySibling(selectedMode) || pipelineMode("1080p60");
      await applyQualityMode(sixty, {
        note: "60 fps on. You can switch back under Quality.",
      });
    } catch (e) {
      log("60 fps failed: " + e);
      setBanner("Could not switch to 60 fps: " + e, true);
    }
  });
  qualityOfferNo.addEventListener("click", function () {
    offer60Dismissed = true;
    qualityOffer.hidden = true;
  });
  if (navigator.mediaDevices && navigator.mediaDevices.addEventListener) {
    navigator.mediaDevices.addEventListener("devicechange", function () {
      listCameras()
        .then(function (cams) {
          sendCameras(cams, cameraSel.value, "");
        })
        .catch(function () {});
    });
  }
  window.addEventListener("pagehide", function (ev) {
    if (ev.persisted) return;
    stopScan();
    if (sessionActive || starting) {
      endSession("", false);
    } else {
      stopTracks();
    }
  });

  fillQualitySelect();
  if (!currentToken()) {
    setStatus("", "Ready");
    setBanner("Type the token from PocketCam on the PC, or tap Scan QR on the PC.");
    log("no token in this URL — enter the code shown on the PC, or scan the QR.");
  } else {
    log("token in URL — tap Start camera, allow access, keep this page open.");
  }
})();
