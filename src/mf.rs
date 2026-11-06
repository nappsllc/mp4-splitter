//! Media Foundation's source reader (from Rustshot's `src/rec/win/mf.rs` and
//! `src/rec/cut/decode.rs`, Apache-2.0): pictures with hardware decoding where
//! the system has it, and sound as floats. The DLLs are loaded at run time:
//! Windows "N" editions ship without them and the app must still start there.

use crate::paint::Picture;
use anyhow::{Context, Result, anyhow, bail};
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear, PropVariantToUInt64};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows::Win32::System::Variant::VT_I8;
use windows::core::{GUID, HRESULT, IUnknown, Interface, PCWSTR, s, w};

const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
const AUDIO: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;

type FnStartup = unsafe extern "system" fn(u32, u32) -> HRESULT;
type FnCreateAttributes = unsafe extern "system" fn(*mut *mut c_void, u32) -> HRESULT;
type FnCreate = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type FnCreateReader = unsafe extern "system" fn(PCWSTR, *mut c_void, *mut *mut c_void) -> HRESULT;
type FnCreateManager = unsafe extern "system" fn(*mut u32, *mut *mut c_void) -> HRESULT;

struct Api {
    create_attributes: FnCreateAttributes,
    create_media_type: FnCreate,
    create_reader: FnCreateReader,
    create_manager: FnCreateManager,
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| unsafe { load() }).as_ref().map_err(|e| anyhow!("{e}"))
}

unsafe fn load() -> std::result::Result<Api, String> {
    // SAFETY: system DLLs, and each symbol's signature is as Media Foundation documents it.
    unsafe {
        let plat: HMODULE = LoadLibraryExW(w!("mfplat.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).map_err(|_| "no mfplat.dll".to_string())?;
        let rw: HMODULE = LoadLibraryExW(w!("mfreadwrite.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).map_err(|_| "no mfreadwrite.dll".to_string())?;
        macro_rules! sym {
            ($m:expr, $name:literal, $t:ty) => {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, $t>(GetProcAddress($m, s!($name)).ok_or_else(|| format!("{} not found", $name))?)
            };
        }
        let startup: FnStartup = sym!(plat, "MFStartup", FnStartup);
        let api = Api {
            create_attributes: sym!(plat, "MFCreateAttributes", FnCreateAttributes),
            create_media_type: sym!(plat, "MFCreateMediaType", FnCreate),
            create_reader: sym!(rw, "MFCreateSourceReaderFromURL", FnCreateReader),
            create_manager: sym!(plat, "MFCreateDXGIDeviceManager", FnCreateManager),
        };
        startup(MF_VERSION, MFSTARTUP_FULL).ok().map_err(|e| format!("MFStartup: {e}"))?;
        Ok(api)
    }
}

/// Can pictures and sound be decoded here?
pub fn available() -> bool {
    api().is_ok()
}

/// Join the multithreaded apartment (once a thread; the objects are free-threaded).
pub fn com_init() {
    thread_local! {
        static INIT: () = {
            // S_FALSE (already) and RPC_E_CHANGED_MODE are both fine.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        };
    }
    INIT.with(|_| ());
}

/// A COM object an out-parameter function makes.
unsafe fn make<T: Interface>(f: impl FnOnce(*mut *mut c_void) -> HRESULT) -> Result<T> {
    let mut p = std::ptr::null_mut();
    f(&mut p).ok()?;
    if p.is_null() {
        bail!("no object");
    }
    // SAFETY: the function gave us one reference to an object of this kind.
    Ok(unsafe { T::from_raw(p) })
}

fn new_type() -> Result<IMFMediaType> {
    let a = api()?;
    // SAFETY: as Media Foundation documents.
    unsafe { make(|p| (a.create_media_type)(p)) }
}

/// The GPU a reader decodes on: a D3D11 device and its DXGI manager, which
/// the reader needs for as long as it lives. It goes with the reader, so an
/// idle app holds no decoding memory.
struct Gpu {
    manager: IMFDXGIDeviceManager,
    _device: ID3D11Device,
}

/// The default adapter's device. `None`: decode in software.
fn gpu() -> Option<Gpu> {
    let a = api().ok()?;
    // SAFETY: as Direct3D and Media Foundation document.
    unsafe {
        let mut device = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .ok()?;
        let device = device?;
        let protect: windows::Win32::Graphics::Direct3D11::ID3D11Multithread = device.cast().ok()?;
        let _ = protect.SetMultithreadProtected(true);
        let mut token = 0u32;
        let manager: IMFDXGIDeviceManager = make(|p| (a.create_manager)(&mut token, p)).ok()?;
        manager.ResetDevice(&device, token).ok()?;
        Some(Gpu { manager, _device: device })
    }
}

/// A source reader on `path`.
fn reader(path: &Path, gpu: Option<&Gpu>) -> Result<IMFSourceReader> {
    com_init();
    let a = api()?;
    let url: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: as Media Foundation documents; `url` outlives the call.
    unsafe {
        let attrs: IMFAttributes = make(|p| (a.create_attributes)(p, 4))?;
        if let Some(g) = gpu {
            attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &g.manager.cast::<IUnknown>()?)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        }
        make(|p| (a.create_reader)(PCWSTR(url.as_ptr()), attrs.as_raw(), p)).context("open the file")
    }
}

/// Seek to the last key frame at or before `hns`. Media Foundation goes to the
/// end for a time past the movie's length, and the frames an edit list shows
/// last can be there: go to just inside, and reading on reaches them.
fn seek(r: &IMFSourceReader, hns: i64) -> Result<()> {
    let hns = match length(r) {
        Some(len) if len > 1 => hns.min(len as i64 - 1),
        _ => hns,
    };
    let mut pv = PROPVARIANT::default();
    // SAFETY: a VT_I8 PROPVARIANT, as the call asks for.
    unsafe {
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_I8;
        inner.Anonymous.hVal = hns.max(0);
        r.SetCurrentPosition(&GUID::zeroed(), &pv)?;
    }
    Ok(())
}

/// The next sample of `stream` and its time, or `None` at the end.
fn next(r: &IMFSourceReader, stream: u32) -> Option<(i64, IMFSample)> {
    // SAFETY: the out-parameters outlive the call.
    unsafe {
        for _ in 0..32 {
            let (mut flags, mut ts, mut sample) = (0u32, 0i64, None);
            r.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample)).ok()?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return None;
            }
            if let Some(s) = sample {
                return Some((ts, s));
            }
        }
        None
    }
}

/// How NV12 turns into BGRA: the colour matrix and the range.
#[derive(Clone, Copy)]
struct Yuv {
    /// Y scale, and the V→R, U→G, V→G and U→B factors, 8.8 fixed.
    ky: i32,
    y0: i32,
    vr: i32,
    ug: i32,
    vg: i32,
    ub: i32,
}

impl Yuv {
    fn of(bt709: bool, full: bool) -> Yuv {
        let (kr, kb) = if bt709 { (0.2126f32, 0.0722f32) } else { (0.299, 0.114) };
        let kg = 1.0 - kr - kb;
        let (sy, sc, y0) = if full { (1.0, 1.0, 0) } else { (255.0 / 219.0, 255.0 / 224.0, 16) };
        let q = |v: f32| (v * 256.0).round() as i32;
        Yuv {
            ky: q(sy),
            y0,
            vr: q(sc * 2.0 * (1.0 - kr)),
            ug: q(sc * 2.0 * (1.0 - kb) * kb / kg),
            vg: q(sc * 2.0 * (1.0 - kr) * kr / kg),
            ub: q(sc * 2.0 * (1.0 - kb)),
        }
    }
}

/// `sw` * `sh` of an NV12 buffer scaled to `w` * `h` BGRA: a 2 x 2 average of
/// the luma when it shrinks, the nearest chroma. (Not a sharper filter: this
/// runs for every frame played, on the CPU, in a size-optimised build.)
#[allow(clippy::too_many_arguments)]
fn nv12_to_bgra(nv: &[u8], pitch: usize, rows: usize, sw: usize, sh: usize, w: usize, h: usize, yuv: Yuv) -> Option<Vec<u8>> {
    let uv0 = pitch * rows;
    if w == 0 || h == 0 || sw == 0 || sh == 0 || nv.len() < uv0 + pitch * sh.div_ceil(2) {
        return None;
    }
    let mut out = vec![0u8; w * h * 4];
    let shrinks = sw >= w * 3 / 2;
    // Each target column's two luma columns and its chroma column.
    let cols: Vec<(u32, u32, u32)> = (0..w)
        .map(|x| {
            let (a, b) = if shrinks {
                (((4 * x + 1) * sw / (4 * w)), ((4 * x + 3) * sw / (4 * w)))
            } else {
                (((2 * x + 1) * sw / (2 * w)), ((2 * x + 1) * sw / (2 * w)))
            };
            (a.min(sw - 1) as u32, b.min(sw - 1) as u32, (((2 * x + 1) * sw / (2 * w)).min(sw - 1) / 2 * 2) as u32)
        })
        .collect();
    let clamp = |v: i32| (v >> 8).clamp(0, 255) as u8;
    for (y, row) in out.chunks_exact_mut(w * 4).enumerate() {
        let (ya, yb) =
            if shrinks { ((4 * y + 1) * sh / (4 * h), (4 * y + 3) * sh / (4 * h)) } else { ((2 * y + 1) * sh / (2 * h), (2 * y + 1) * sh / (2 * h)) };
        let (la, lb) = (ya.min(sh - 1) * pitch, yb.min(sh - 1) * pitch);
        let cr = uv0 + (((2 * y + 1) * sh / (2 * h)).min(sh - 1) / 2) * pitch;
        for (o, &(a, b, c)) in row.chunks_exact_mut(4).zip(&cols) {
            let (a, b, c) = (a as usize, b as usize, c as usize);
            let l = if shrinks { (nv[la + a] as i32 + nv[la + b] as i32 + nv[lb + a] as i32 + nv[lb + b] as i32 + 2) >> 2 } else { nv[la + a] as i32 };
            let (u, v) = (nv[cr + c] as i32 - 128, nv[cr + c + 1] as i32 - 128);
            let k = (l - yuv.y0) * yuv.ky + 128;
            o[0] = clamp(k + yuv.ub * u);
            o[1] = clamp(k - yuv.ug * u - yuv.vg * v);
            o[2] = clamp(k + yuv.vr * v);
            o[3] = 255;
        }
    }
    Some(out)
}

/// `p` turned clockwise by `quarters` right angles.
fn turned(src: Vec<u8>, w: usize, h: usize, quarters: u8) -> Vec<u8> {
    if quarters.is_multiple_of(4) {
        return src;
    }
    let mut out = vec![0u8; src.len()];
    let (dw, _) = if quarters % 2 == 1 { (h, w) } else { (w, h) };
    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = match quarters % 4 {
                1 => (h - 1 - y, x),
                2 => (w - 1 - x, h - 1 - y),
                _ => (y, w - 1 - x),
            };
            let (from, to) = ((y * w + x) * 4, (dy * dw + dx) * 4);
            out[to..to + 4].copy_from_slice(&src[from..from + 4]);
        }
    }
    out
}

/// One decoded picture, not yet converted.
pub struct Decoded {
    /// The time Media Foundation gives it (100 ns).
    pub ts: i64,
    sample: IMFSample,
}

/// Reads a file's pictures; kept open and seeking in place.
pub struct VideoReader {
    r: IMFSourceReader,
    /// The coded size (rows of luma before the chroma), the picture in it.
    coded: (u32, u32),
    shown: (u32, u32),
    stride: usize,
    yuv: Yuv,
    /// Quarter turns clockwise to show it upright.
    turn: u8,
    _gpu: Option<Gpu>,
}

// SAFETY: Media Foundation's objects are free-threaded; used by one thread at a time.
unsafe impl Send for VideoReader {}

impl VideoReader {
    pub fn open(path: &Path, hardware: bool, turn: u8) -> Result<VideoReader> {
        com_init();
        let gpu = if hardware { gpu() } else { None };
        let r = reader(path, gpu.as_ref())?;
        // SAFETY: as Media Foundation documents.
        unsafe {
            let _ = r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
            r.SetStreamSelection(VIDEO, true).context("no video stream")?;
            let want = new_type()?;
            want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            r.SetCurrentMediaType(VIDEO, None, &want).context("no decoder for this video")?;
            let cur = r.GetCurrentMediaType(VIDEO)?;
            let fs = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
            let coded = ((fs >> 32) as u32, fs as u32);
            // The picture inside the (aligned) coded size.
            let mut shown = coded;
            let mut area = MFVideoArea::default();
            let raw = std::slice::from_raw_parts_mut((&raw mut area).cast::<u8>(), std::mem::size_of::<MFVideoArea>());
            if cur.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, raw, None).is_ok() && area.Area.cx > 0 && area.Area.cy > 0 {
                shown = (area.Area.cx as u32, area.Area.cy as u32);
            }
            let stride = cur.GetUINT32(&MF_MT_DEFAULT_STRIDE).map_or(coded.0 as usize, |s| (s as i32).unsigned_abs() as usize);
            let matrix = cur.GetUINT32(&MF_MT_YUV_MATRIX).unwrap_or(0);
            let range = cur.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE).unwrap_or(0);
            let bt709 = if matrix == MFVideoTransferMatrix_BT601.0 as u32 { false } else { matrix == MFVideoTransferMatrix_BT709.0 as u32 || shown.1 >= 720 };
            let yuv = Yuv::of(bt709, range == MFNominalRange_0_255.0 as u32);
            Ok(VideoReader { r, coded, shown, stride, yuv, turn, _gpu: gpu })
        }
    }

    pub fn seek(&mut self, hns: i64) -> Result<()> {
        seek(&self.r, hns)
    }

    /// The next picture in presentation order.
    pub fn next(&mut self) -> Option<Decoded> {
        next(&self.r, VIDEO).map(|(ts, sample)| Decoded { ts, sample })
    }

    /// The picture as BGRA, scaled to `size`.
    pub fn picture(&self, d: &Decoded, size: (u32, u32)) -> Option<Picture> {
        let (w, h) = (size.0 as usize, size.1 as usize);
        // Scaled before it is turned: `size` is as shown, upright.
        let (rw, rh) = if self.turn % 2 == 1 { (h, w) } else { (w, h) };
        // SAFETY: the buffer stays locked while its bytes are read.
        let px = unsafe {
            let buf = d.sample.GetBufferByIndex(0).ok()?;
            if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                let (mut data, mut pitch) = (std::ptr::null_mut(), 0i32);
                let total = b2.GetContiguousLength().ok()? as usize;
                b2.Lock2D(&mut data, &mut pitch).ok()?;
                let pitch = pitch.unsigned_abs() as usize;
                // The decoder's surfaces can be taller than the picture (1088
                // rows for 1080): the chroma starts after all of them.
                let rows = (total * 2 / 3 / pitch.max(1)).max(self.coded.1 as usize);
                let nv = std::slice::from_raw_parts(data, pitch * rows + pitch * rows.div_ceil(2));
                let out = nv12_to_bgra(nv, pitch, rows, self.shown.0 as usize, self.shown.1 as usize, rw, rh, self.yuv);
                let _ = b2.Unlock2D();
                out
            } else {
                let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
                buf.Lock(&mut data, None, Some(&mut len)).ok()?;
                let nv = std::slice::from_raw_parts(data, len as usize);
                let out = nv12_to_bgra(nv, self.stride, self.coded.1 as usize, self.shown.0 as usize, self.shown.1 as usize, rw, rh, self.yuv);
                let _ = buf.Unlock();
                out
            }
        }?;
        Some(Picture { w: size.0, h: size.1, bgra: Arc::new(turned(px, rw, rh, self.turn)) })
    }
}

/// Reads a file's sound as interleaved floats.
pub struct AudioReader {
    r: IMFSourceReader,
    pub rate: u32,
    pub channels: u16,
}

// SAFETY: Media Foundation's objects are free-threaded; used by one thread at a time.
unsafe impl Send for AudioReader {}

impl AudioReader {
    pub fn open(path: &Path) -> Result<AudioReader> {
        let r = reader(path, None)?;
        // SAFETY: as Media Foundation documents.
        unsafe {
            let _ = r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
            r.SetStreamSelection(AUDIO, true).context("no sound stream")?;
            let want = new_type()?;
            want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            want.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)?;
            r.SetCurrentMediaType(AUDIO, None, &want).context("no decoder for this sound")?;
            let cur = r.GetCurrentMediaType(AUDIO)?;
            let rate = cur.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)?;
            let channels = cur.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)? as u16;
            Ok(AudioReader { r, rate, channels })
        }
    }

    pub fn seek(&mut self, hns: i64) -> Result<()> {
        seek(&self.r, hns)
    }

    /// The next packet's time (100 ns) and samples (interleaved).
    pub fn next(&mut self) -> Option<(i64, Vec<f32>)> {
        let (ts, sample) = next(&self.r, AUDIO)?;
        // SAFETY: the buffer stays locked while its bytes are read.
        unsafe {
            let buf = sample.ConvertToContiguousBuffer().ok()?;
            let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
            buf.Lock(&mut data, None, Some(&mut len)).ok()?;
            let v = std::slice::from_raw_parts(data.cast::<u8>(), len as usize).chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
            let _ = buf.Unlock();
            Some((ts, v))
        }
    }
}

/// How long the file is, as Media Foundation sees it (100 ns).
fn length(r: &IMFSourceReader) -> Option<u64> {
    // SAFETY: as Media Foundation documents.
    unsafe {
        let mut pv = r.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION).ok()?;
        let hns = PropVariantToUInt64(&pv).ok();
        let _ = PropVariantClear(&mut pv);
        hns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_converts_to_bgra() {
        // White, black, and neutral chroma.
        let nv = vec![235, 16, 235, 16, 128, 128];
        let p = nv12_to_bgra(&nv, 2, 2, 2, 2, 2, 2, Yuv::of(true, false)).unwrap();
        assert_eq!(&p[..4], &[255, 255, 255, 255]);
        assert_eq!(&p[4..8], &[0, 0, 0, 255]);
        assert!(nv12_to_bgra(&nv[..4], 2, 2, 2, 2, 2, 2, Yuv::of(true, false)).is_none(), "too short");
    }

    #[test]
    fn pictures_turn_clockwise() {
        // 2 x 1: a, b -> 1 x 2: a on top after a quarter turn.
        let (a, b) = ([1u8, 1, 1, 255], [2u8, 2, 2, 255]);
        let src = [a, b].concat();
        assert_eq!(turned(src.clone(), 2, 1, 1), [a, b].concat());
        assert_eq!(turned(src.clone(), 2, 1, 2), [b, a].concat());
        assert_eq!(turned(src, 2, 1, 3), [b, a].concat());
    }

    /// What times Media Foundation gives files with and without a hidden
    /// lead-in (run with --nocapture): how its clock relates to the file's.
    #[test]
    #[ignore = "exploration"]
    fn explore_times() {
        let ff = crate::ffmpeg::find().unwrap();
        let dir = std::env::temp_dir().join(format!("mp4s-mf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        assert!(
            std::process::Command::new(&ff)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=320x240:rate=30:duration=6",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=6",
                    "-c:v",
                    "libx264",
                    "-g",
                    "30",
                    "-bf",
                    "2",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-shortest"
                ])
                .arg(&src)
                .status()
                .unwrap()
                .success()
        );
        let cut = dir.join("cut.mp4");
        let mut r = std::io::BufReader::new(std::fs::File::open(&src).unwrap());
        let mut w = std::io::BufWriter::new(std::fs::File::create(&cut).unwrap());
        crate::mp4::trim(&mut r, &mut w, std::time::Duration::from_secs_f64(1.5), std::time::Duration::from_secs_f64(4.0)).unwrap();
        drop(w);
        for (name, p) in [("plain", &src), ("cut 1.5..4", &cut)] {
            let info = crate::mp4::info(&mut std::io::BufReader::new(std::fs::File::open(p).unwrap())).unwrap();
            println!("{name}: info frames {:?}.. of {}; keys {:?}", &info.frames[..3], info.frames.len(), &info.keys[..2]);
            for hw in [false, true] {
                let mut v = VideoReader::open(p, hw, 0).unwrap();
                println!("  hw={hw} coded {:?} shown {:?} stride {}", v.coded, v.shown, v.stride);
                let mut got = Vec::new();
                for _ in 0..4 {
                    got.push(v.next().unwrap().ts as f64 / 1e7);
                }
                println!("  first: {got:?}");
                v.seek(20_000_000).unwrap();
                let mut got = Vec::new();
                for _ in 0..4 {
                    got.push(v.next().unwrap().ts as f64 / 1e7);
                }
                println!("  after seek 2.0: {got:?}");
                let t = std::time::Instant::now();
                let mut n = 0;
                v.seek(0).unwrap();
                while n < 60 {
                    let d = v.next().unwrap();
                    let _ = v.picture(&d, (320, 240)).unwrap();
                    n += 1;
                }
                println!("  60 frames: {:?}", t.elapsed());
            }
            let mut a = AudioReader::open(p).unwrap();
            let (ts, s) = a.next().unwrap();
            println!("  audio {} Hz {} ch, first packet at {} s, {} samples", a.rate, a.channels, ts as f64 / 1e7, s.len());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
