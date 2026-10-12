#[derive(Clone, Copy)]
pub(super) enum Qos {
    Background,
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Qos;

    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }

    pub(super) fn set_self(qos: Qos) {
        match qos {
            Qos::Background => {
                // SAFETY: this applies a scheduling hint to the calling thread.
                unsafe { pthread_set_qos_class_self_np(0x09, 0) };
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::Qos;

    pub(super) fn set_self(_qos: Qos) {}
}

pub(super) fn set_self(qos: Qos) {
    imp::set_self(qos);
}
