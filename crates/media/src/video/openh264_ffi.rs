//! Minimal direct binding to the openh264 encoder.
//!
//! The safe `openh264` crate always configures a single slice, and openh264
//! only parallelizes across slices, so its thread setting has no effect. At
//! 1440p that leaves real desktop content at ~28 ms/frame on one core. This
//! wrapper configures one slice per thread (`SM_FIXEDSLCNUM_SLICE`).
//!
//! All `unsafe` is confined to this module.
#![allow(unsafe_code)]

use std::{os::raw::c_int, ptr::null_mut};

use openh264_sys2::{
    API, CONSTANT_ID, DEBLOCKING_IDC_0, ENCODER_OPTION_DATAFORMAT, ENCODER_OPTION_TRACE_LEVEL,
    ISVCEncoder, ISVCEncoderVtbl, LEVEL_5_1, LOW_COMPLEXITY, PRO_BASELINE, RC_BITRATE_MODE,
    SCREEN_CONTENT_REAL_TIME, SEncParamExt, SFrameBSInfo, SM_FIXEDSLCNUM_SLICE, SSourcePicture,
    WELS_LOG_ERROR, source::APILoader, videoFormatI420, videoFrameTypeI, videoFrameTypeIDR,
    videoFrameTypeIPMixed, videoFrameTypeP,
};

pub struct EncoderParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub intra_period_frames: u32,
    /// Encoder threads; also the number of slices per frame.
    pub threads: u16,
}

/// A borrowed I420 picture.
pub struct Planes<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub strides: (usize, usize, usize),
}

pub struct RawEncoder {
    enc: *mut ISVCEncoder,
    info: Box<SFrameBSInfo>,
    width: u32,
    height: u32,
}

// SAFETY: the encoder instance has no thread affinity; `&mut self` on every
// call guarantees it is used from one thread at a time.
unsafe impl Send for RawEncoder {}

fn check(rc: c_int, what: &str) -> Result<(), String> {
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("openh264 {what} failed ({rc})"))
    }
}

impl RawEncoder {
    pub fn new(p: &EncoderParams) -> Result<Self, String> {
        if p.width == 0
            || p.height == 0
            || !p.width.is_multiple_of(2)
            || !p.height.is_multiple_of(2)
        {
            return Err(format!("invalid size {}x{}", p.width, p.height));
        }
        let mut enc: *mut ISVCEncoder = null_mut();
        // SAFETY: `enc` is a valid out-pointer.
        check(
            unsafe { APILoader::new().WelsCreateSVCEncoder(&raw mut enc) },
            "create",
        )?;
        if enc.is_null() {
            return Err("openh264 returned a null encoder".into());
        }
        let this = Self {
            enc,
            info: Box::default(),
            width: p.width,
            height: p.height,
        };
        let vt = this.vtable();
        let get_default = vt.GetDefaultParams.ok_or("missing GetDefaultParams")?;
        let init = vt.InitializeExt.ok_or("missing InitializeExt")?;
        let set_option = vt.SetOption.ok_or("missing SetOption")?;

        let mut params = SEncParamExt::default();
        // SAFETY: valid encoder and param struct pointers.
        check(
            unsafe { get_default(enc, &raw mut params) },
            "GetDefaultParams",
        )?;
        let bitrate = c_int::try_from(p.bitrate_bps).map_err(|e| e.to_string())?;
        params.iUsageType = SCREEN_CONTENT_REAL_TIME;
        params.iPicWidth = p.width as c_int;
        params.iPicHeight = p.height as c_int;
        params.iTargetBitrate = bitrate;
        params.iMaxBitrate = bitrate.saturating_mul(3) / 2;
        params.iRCMode = RC_BITRATE_MODE;
        params.fMaxFrameRate = p.fps as f32;
        params.bEnableFrameSkip = true;
        params.bEnableDenoise = false;
        params.bEnableSceneChangeDetect = true;
        params.bEnableAdaptiveQuant = false; // unsupported for screen content
        params.bEnableBackgroundDetection = false; // unsupported for screen content
        params.bEnableLongTermReference = false;
        params.iComplexityMode = LOW_COMPLEXITY;
        params.uiIntraPeriod = p.intra_period_frames;
        params.eSpsPpsIdStrategy = CONSTANT_ID;
        params.iLoopFilterDisableIdc = DEBLOCKING_IDC_0 as c_int;
        params.iMultipleThreadIdc = p.threads.max(1);
        params.iSpatialLayerNum = 1;
        params.iTemporalLayerNum = 1;
        let layer = &mut params.sSpatialLayers[0];
        layer.iVideoWidth = p.width as c_int;
        layer.iVideoHeight = p.height as c_int;
        layer.fFrameRate = p.fps as f32;
        layer.iSpatialBitrate = bitrate;
        layer.iMaxSpatialBitrate = bitrate.saturating_mul(3) / 2;
        layer.uiProfileIdc = PRO_BASELINE;
        layer.uiLevelIdc = LEVEL_5_1;
        layer.sSliceArgument.uiSliceMode = SM_FIXEDSLCNUM_SLICE;
        layer.sSliceArgument.uiSliceNum = u32::from(p.threads.max(1));

        // SAFETY: valid encoder; params fully initialized above.
        check(unsafe { init(enc, &raw const params) }, "InitializeExt")?;
        let mut format: c_int = videoFormatI420 as c_int;
        let mut trace: c_int = WELS_LOG_ERROR as c_int;
        // SAFETY: options take a pointer to an int, valid for the call.
        unsafe {
            check(
                set_option(enc, ENCODER_OPTION_DATAFORMAT, (&raw mut format).cast()),
                "DATAFORMAT",
            )?;
            let _ = set_option(enc, ENCODER_OPTION_TRACE_LEVEL, (&raw mut trace).cast());
        }
        Ok(this)
    }

    fn vtable(&self) -> &ISVCEncoderVtbl {
        // SAFETY: `enc` is non-null and points at openh264's vtable pointer,
        // which lives as long as the encoder.
        unsafe { &**self.enc }
    }

    pub fn force_intra_frame(&mut self) {
        if let Some(f) = self.vtable().ForceIntraFrame {
            // SAFETY: valid, initialized encoder.
            unsafe { f(self.enc, true) };
        }
    }

    /// Encodes one picture. Returns `(annex_b_bytes, keyframe)` or `None` if the
    /// rate controller skipped the frame.
    pub fn encode(
        &mut self,
        pic: &Planes<'_>,
        timestamp_ms: i64,
    ) -> Result<Option<(Vec<u8>, bool)>, String> {
        let (w, h) = (self.width as usize, self.height as usize);
        let (ys, us, vs) = pic.strides;
        if pic.y.len() < ys * h
            || pic.u.len() < us * (h / 2)
            || pic.v.len() < vs * (h / 2)
            || ys < w
            || us < w / 2
        {
            return Err("plane buffers too small".into());
        }
        let encode = self.vtable().EncodeFrame.ok_or("missing EncodeFrame")?;
        let source = SSourcePicture {
            iColorFormat: videoFormatI420 as c_int,
            iStride: [ys as c_int, us as c_int, vs as c_int, 0],
            // The encoder only reads the planes; the API just isn't const-correct.
            pData: [
                pic.y.as_ptr().cast_mut(),
                pic.u.as_ptr().cast_mut(),
                pic.v.as_ptr().cast_mut(),
                null_mut(),
            ],
            iPicWidth: w as c_int,
            iPicHeight: h as c_int,
            uiTimeStamp: timestamp_ms,
            bPsnrY: false,
            bPsnrU: false,
            bPsnrV: false,
        };
        // SAFETY: plane sizes checked above; `info` is a valid out struct owned by us.
        check(
            unsafe { encode(self.enc, &raw const source, &raw mut *self.info) },
            "EncodeFrame",
        )?;

        let keyframe = match self.info.eFrameType {
            t if t == videoFrameTypeIDR || t == videoFrameTypeI => true,
            t if t == videoFrameTypeP || t == videoFrameTypeIPMixed => false,
            _ => return Ok(None), // skip / invalid
        };
        let mut out = Vec::with_capacity(self.info.iFrameSizeInBytes.max(0) as usize);
        let layers = (self.info.iLayerNum.max(0) as usize).min(self.info.sLayerInfo.len());
        for layer in &self.info.sLayerInfo[..layers] {
            let nals = layer.iNalCount.max(0) as usize;
            if nals == 0 || layer.pBsBuf.is_null() || layer.pNalLengthInByte.is_null() {
                continue;
            }
            // SAFETY: openh264 guarantees `pNalLengthInByte` has `iNalCount`
            // entries and the NALs are contiguous in `pBsBuf`; valid until the
            // next EncodeFrame call, and we copy out immediately.
            unsafe {
                let lens = std::slice::from_raw_parts(layer.pNalLengthInByte, nals);
                let total: usize = lens.iter().map(|l| (*l).max(0) as usize).sum();
                out.extend_from_slice(std::slice::from_raw_parts(layer.pBsBuf, total));
            }
        }
        if out.is_empty() {
            return Ok(None);
        }
        Ok(Some((out, keyframe)))
    }
}

impl Drop for RawEncoder {
    fn drop(&mut self) {
        if let Some(uninit) = self.vtable().Uninitialize {
            // SAFETY: valid encoder, not used after this point.
            unsafe { uninit(self.enc) };
        }
        // SAFETY: created by WelsCreateSVCEncoder, destroyed exactly once.
        unsafe { APILoader::new().WelsDestroySVCEncoder(self.enc) };
    }
}
