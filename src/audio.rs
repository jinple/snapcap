//! Per-application audio via WASAPI process-loopback capture (Windows 10 2004+).
//! Captures only the target process (and its child processes), nothing else.
//! Packets are pushed through a channel; the video thread owns the sink writer.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::*;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::*;
use windows::core::{GUID, Interface, Ref, Result, implement, w};

pub const RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;

pub struct Chunk {
    /// 16-bit interleaved stereo PCM.
    pub pcm: Vec<u8>,
    /// 100ns units since recording start.
    pub time: i64,
    pub dur: i64,
}

pub struct Audio {
    pub rx: Receiver<Chunk>,
    stop: Arc<AtomicBool>,
    t0_tx: Sender<Instant>,
    thread: Option<JoinHandle<()>>,
}

impl Audio {
    pub fn begin(&self, t0: Instant) {
        let _ = self.t0_tx.send(t0);
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Handler(Mutex<Sender<()>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Handler_Impl {
    fn ActivateCompleted(&self, _op: Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        let _ = self.0.lock().unwrap().send(());
        Ok(())
    }
}

/// PROPVARIANT(VT_BLOB) laid out by hand (x64): vt + 3 reserved words, then {cb, ptr}.
#[repr(C)]
struct Blob {
    vt: u16,
    reserved: [u16; 3],
    cb: u32,
    pad: u32,
    data: *const c_void,
}

unsafe fn open(pid: u32) -> Result<(IAudioClient, IAudioCaptureClient, HANDLE)> {
    unsafe {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        let pv = Blob {
            vt: 65, // VT_BLOB
            reserved: [0; 3],
            cb: size_of_val(&params) as u32,
            pad: 0,
            data: &params as *const _ as *const c_void,
        };
        let (tx, rx) = mpsc::channel();
        let handler: IActivateAudioInterfaceCompletionHandler = Handler(Mutex::new(tx)).into();
        let iid: GUID = IAudioClient::IID;
        let op = ActivateAudioInterfaceAsync(
            w!("VAD\\Process_Loopback"),
            &iid,
            Some(&pv as *const Blob as *const PROPVARIANT),
            &handler,
        )?;
        rx.recv_timeout(Duration::from_secs(3)).map_err(|_| windows::core::Error::from(E_FAIL))?;
        let mut hr = windows::core::HRESULT(0);
        let mut unk: Option<windows::core::IUnknown> = None;
        op.GetActivateResult(&mut hr, &mut unk)?;
        hr.ok()?;
        let client: IAudioClient = unk.ok_or_else(|| windows::core::Error::from(E_FAIL))?.cast()?;

        let fmt = WAVEFORMATEX {
            wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
            nChannels: CHANNELS as u16,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * CHANNELS * 4,
            nBlockAlign: (CHANNELS * 4) as u16,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        // LOOPBACK | EVENTCALLBACK | AUTOCONVERTPCM | SRC_DEFAULT_QUALITY
        let flags = 0x0002_0000 | 0x0004_0000 | 0x8000_0000 | 0x0800_0000;
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &fmt, None)?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let cap: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok((client, cap, event))
    }
}

/// Opens the capture stream. Call `Audio::begin` with the shared recording clock
/// once everything else is ready; audio before that moment is discarded.
pub fn start(pid: u32) -> Result<Audio> {
    let (t0_tx, t0_rx) = mpsc::channel::<Instant>();
    let (init_tx, init_rx) = mpsc::channel::<Result<()>>();
    let (tx, rx) = mpsc::channel::<Chunk>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let (client, cap, event) = match open(pid) {
            Ok(v) => {
                let _ = init_tx.send(Ok(()));
                v
            }
            Err(e) => {
                let _ = init_tx.send(Err(e));
                return;
            }
        };
        let Ok(t0) = t0_rx.recv() else { return };
        // QPC (100ns) value corresponding to recording start
        let (mut now_qpc, mut freq) = (0i64, 0i64);
        let _ = QueryPerformanceCounter(&mut now_qpc);
        let _ = QueryPerformanceFrequency(&mut freq);
        let now_100ns = (now_qpc as i128 * 10_000_000 / freq.max(1) as i128) as i64;
        let base_qpc = now_100ns - (t0.elapsed().as_nanos() / 100) as i64;
        let mut written: u64 = 0; // frames sent so far
        let ts = |frames: u64| (frames as i128 * 10_000_000 / RATE as i128) as i64;
        while !stop2.load(Ordering::Relaxed) {
            WaitForSingleObject(event, 50);
            loop {
                let n = match cap.GetNextPacketSize() {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if n == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64; // capture time of the first frame, 100ns QPC units
                if cap.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)).is_err() {
                    break;
                }
                let samples = frames as usize * CHANNELS as usize;
                let mut pcm = Vec::with_capacity(samples * 2);
                if flags & 0x2 != 0 || data.is_null() {
                    pcm.resize(samples * 2, 0); // AUDCLNT_BUFFERFLAGS_SILENT
                } else {
                    let f = std::slice::from_raw_parts(data as *const f32, samples);
                    for s in f {
                        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        pcm.extend_from_slice(&v.to_le_bytes());
                    }
                }
                let _ = cap.ReleaseBuffer(frames);

                // The process may have been silent (no packets); keep audio on the wall clock.
                // Use the packet's own capture timestamp, not arrival time, so buffering
                // latency can't be mistaken for a gap.
                let pkt_t = (qpc as i64 - base_qpc) as f64 / 1e7;
                if pkt_t < 0.0 {
                    continue; // captured before the recording clock started
                }
                let lag = pkt_t - written as f64 / RATE as f64;
                if lag > 0.02 {
                    let gap = (lag * RATE as f64) as u64;
                    let _ = tx.send(Chunk {
                        pcm: vec![0; gap as usize * CHANNELS as usize * 2],
                        time: ts(written),
                        dur: ts(gap),
                    });
                    written += gap;
                }
                let _ = tx.send(Chunk { pcm, time: ts(written), dur: ts(frames as u64) });
                written += frames as u64;
            }
        }
        let _ = client.Stop();
        let _ = CloseHandle(event);
        CoUninitialize();
    });
    match init_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(Audio { rx, stop, t0_tx, thread: Some(thread) }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(windows::core::Error::from(E_FAIL)),
    }
}
