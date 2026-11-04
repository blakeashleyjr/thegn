//! Resident model for one utterance, reused for rolling windows and final pass.
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

pub struct Engine {
    context: WhisperContext,
}

struct Abort {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

unsafe extern "C" fn should_abort(data: *mut std::ffi::c_void) -> bool {
    if data.is_null() {
        return true;
    }
    // SAFETY: transcribe supplies an immutable Abort that outlives the entire
    // synchronous full() call. Its fields are safe to read from native threads.
    let abort = unsafe { &*data.cast::<Abort>() };
    abort.cancelled.load(Ordering::Acquire) || Instant::now() >= abort.deadline
}
impl Engine {
    pub fn validate_language(language: &str) -> Result<()> {
        if language.contains('\0')
            || (language != "auto"
                && !language.is_empty()
                && whisper_rs::get_lang_id(language).is_none())
        {
            bail!("unsupported Whisper language code");
        }
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        let path = path.to_str().context("model path must be UTF-8")?;
        let mut params = WhisperContextParameters::default();
        params.use_gpu(cfg!(all(target_os = "macos", feature = "metal")));
        let context = WhisperContext::new_with_params(path, params)
            .and_then(|context| {
                if cfg!(all(target_os = "macos", feature = "metal")) {
                    // The no-state loader can succeed before Metal compute
                    // buffers are allocated. Preflight that allocation too so
                    // startup can fall back to CPU before opening the mic.
                    context.create_state()?;
                }
                Ok(context)
            })
            .or_else(|error| {
                if cfg!(all(target_os = "macos", feature = "metal")) {
                    let mut cpu = WhisperContextParameters::default();
                    cpu.use_gpu(false);
                    WhisperContext::new_with_params(path, cpu)
                } else {
                    Err(error)
                }
            })
            .context("load Whisper model (Metal with CPU fallback on macOS; CPU on Linux)")?;
        Ok(Self { context })
    }
    pub fn transcribe(
        &self,
        samples: &[f32],
        language: &str,
        cancelled: Arc<AtomicBool>,
        timeout: Duration,
    ) -> Result<String> {
        if cancelled.load(Ordering::Acquire) {
            bail!("Whisper transcription cancelled");
        }
        if samples.is_empty() {
            return Ok(String::new());
        }
        Self::validate_language(language)?;
        // Avoid Whisper's tendency to hallucinate on digital silence.
        if samples.iter().map(|s| s * s).sum::<f32>() / (samples.len() as f32) < 0.000001 {
            return Ok(String::new());
        }
        // Declare the callback context before `state`: ggml stores this
        // pointer in the state backend, so the context must outlive the state.
        let abort = Abort {
            cancelled: cancelled.clone(),
            deadline: Instant::now() + timeout,
        };
        let mut state = self.context.create_state()?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(
            std::thread::available_parallelism().map_or(2, |n| n.get().min(4)) as i32,
        );
        params.set_language(if language == "auto" || language.is_empty() {
            None
        } else {
            Some(language)
        });
        params.set_translate(false);
        params.set_no_context(true);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        // whisper-rs 0.16.0's safe helper double-boxes a trait object, then
        // casts that allocation to the original closure type in its trampoline.
        // Use a concrete, owned callback context instead (and avoid its leak).
        // SAFETY: ggml retains the callback pointer in this state's backend
        // until the state is dropped. `abort` was declared before `state`, so
        // it outlives that pointer; it is immutable during inference and the
        // callback never accesses Whisper state, mutates data or panics.
        unsafe {
            params.set_abort_callback(Some(should_abort));
            params.set_abort_callback_user_data(std::ptr::from_ref(&abort).cast_mut().cast());
        }
        let result = state.full(params, samples);
        if cancelled.load(Ordering::Acquire) || Instant::now() >= abort.deadline {
            bail!("Whisper transcription cancelled or timed out");
        }
        result?;
        Ok(state
            .as_iter()
            .map(|s| s.to_str_lossy().map(|text| text.into_owned()))
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join(""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_abort_context_observes_flag_and_deadline_without_ownership_transfer() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut abort = Abort {
            cancelled: flag.clone(),
            deadline: Instant::now() + Duration::from_secs(60),
        };
        let data = std::ptr::from_mut(&mut abort).cast();
        // SAFETY: each callback receives a live Abort and returns synchronously.
        unsafe {
            assert!(!should_abort(data));
            flag.store(true, Ordering::Release);
            assert!(should_abort(data));
            flag.store(false, Ordering::Release);
            abort.deadline = Instant::now();
            assert!(should_abort(std::ptr::from_mut(&mut abort).cast()));
        }
        assert_eq!(Arc::strong_count(&flag), 2);
    }
}
