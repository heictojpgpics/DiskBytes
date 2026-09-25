//! COM STA apartment guard for shell/recycle calls.

/// COM apartment initialization guard (recycle thread). `CoUninitialize`
/// runs on drop EXACTLY once per successful `CoInitializeEx` (including
/// the S_FALSE "already initialized" case — balancing is required).
pub struct ComApartment {
    /// The HRESULT returned by `CoInitializeEx` (S_FALSE = already init).
    hr: windows::core::HRESULT,
}

impl ComApartment {
    /// Initialize COM on the current thread (apartment-threaded,
    /// OLE1DDE disabled — the shell's documented requirement).
    ///
    /// # Errors
    /// User-readable message when COM cannot initialize.
    pub fn init() -> Result<Self, String> {
        use windows::Win32::System::Com::{
            CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
        };
        // SAFETY: no reserved params; the thread has no prior unbalanced
        // init (the guard owns the pairing).
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if hr.is_ok() || hr == windows::core::HRESULT(1)
        /* S_FALSE */
        {
            Ok(Self { hr })
        } else {
            Err(format!("Windows COM unavailable (0x{:08X})", hr.0 as u32))
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: balances the CoInitializeEx from init() on THIS thread.
        unsafe { windows::Win32::System::Com::CoUninitialize() };
        let _ = self.hr;
    }
}
