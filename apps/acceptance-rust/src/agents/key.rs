pub(super) const CAN_SAVE: bool = cfg!(windows);
pub(super) const MAX_LENGTH: usize = 1024;
pub(super) const UNREADABLE: &str = "无法读取已保存的 API Key，请重新填写。";

#[cfg(windows)]
pub(super) fn protect(value: &str, endpoint: &str) -> Result<String, &'static str> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    dpapi(value.as_bytes(), endpoint, true)
        .map(|bytes| STANDARD.encode(bytes))
        .ok_or("无法加密保存 API Key，请确认服务由当前 Windows 用户运行。")
}

#[cfg(windows)]
pub(super) fn unprotect(value: &str, endpoint: &str) -> Result<String, &'static str> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    STANDARD
        .decode(value)
        .ok()
        .and_then(|bytes| dpapi(&bytes, endpoint, false))
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or(UNREADABLE)
}

#[cfg(windows)]
fn dpapi(bytes: &[u8], endpoint: &str, encrypt: bool) -> Option<Vec<u8>> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };
    use zeroize::Zeroize as _;

    let blob = |data: &[u8]| {
        Some(CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(data.len()).ok()?,
            pbData: data.as_ptr().cast_mut(),
        })
    };
    let input = blob(bytes)?;
    let entropy = blob(endpoint.as_bytes())?;
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: null_mut(),
    };
    // The supplied buffers stay live for the call. DPAPI allocates output for LocalFree.
    unsafe {
        let ok = if encrypt {
            CryptProtectData(
                &input,
                null(),
                &entropy,
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                null_mut(),
                &entropy,
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 || output.pbData.is_null() {
            return None;
        }
        let buffer = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
        let result = buffer.to_vec();
        buffer.zeroize();
        LocalFree(output.pbData.cast());
        Some(result)
    }
}

#[cfg(not(windows))]
pub(super) fn protect(_: &str, _: &str) -> Result<String, &'static str> {
    Err("当前部署请通过服务端配置 API Key。")
}

#[cfg(not(windows))]
pub(super) fn unprotect(_: &str, _: &str) -> Result<String, &'static str> {
    Err(UNREADABLE)
}
