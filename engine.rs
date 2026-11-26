use serde_json::Value;
use std::ffi::{CStr, CString};
use std::ptr;

fn take_c_string(pointer: *mut std::os::raw::c_char) -> Result<String, String> {
    if pointer.is_null() {
        return Err("Hanabi engine returned an empty response".into());
    }
    // The engine transfers ownership of every returned string to the caller.
    let value = unsafe { CStr::from_ptr(pointer).to_string_lossy().into_owned() };
    unsafe { hanabi_engine::hanabi_engine_free(pointer) };
    Ok(value)
}

pub fn request(request: Value) -> Result<Value, String> {
    let input = CString::new(request.to_string()).map_err(|e| e.to_string())?;
    let output = unsafe { hanabi_engine::hanabi_engine_request(input.as_ptr()) };
    let text = take_c_string(output)?;
    let response: Value = serde_json::from_str(&text).map_err(|e| format!("Invalid engine response: {e}"))?;
    if !response["ok"].as_bool().unwrap_or(false) {
        return Err(response["error"].as_str().unwrap_or("Engine request failed").to_owned());
    }
    Ok(response["result"].clone())
}

pub fn submit(request: Value) -> Result<u64, String> {
    let input = CString::new(request.to_string()).map_err(|e| e.to_string())?;
    let id = unsafe { hanabi_engine::hanabi_engine_submit(input.as_ptr()) };
    if id == 0 {
        Err("The media engine is busy or could not queue this task".into())
    } else {
        Ok(id)
    }
}

pub fn poll() -> Result<Option<Value>, String> {
    let pointer = hanabi_engine::hanabi_engine_poll();
    if pointer.is_null() {
        return Ok(None);
    }
    let text = take_c_string(pointer)?;
    let value = serde_json::from_str(&text).map_err(|e| format!("Invalid background response: {e}"))?;
    Ok(Some(value))
}

pub fn copy_preview_frame(request_id: u64) -> Result<(u32, u32, f64, Vec<u8>), String> {
    let mut width = 0_u32;
    let mut height = 0_u32;
    let mut byte_len = 0_usize;
    let mut at = 0.0_f64;
    let probe = unsafe {
        hanabi_engine::hanabi_engine_copy_preview_frame(
            request_id,
            ptr::null_mut(),
            0,
            &mut width,
            &mut height,
            &mut byte_len,
            &mut at,
        )
    };
    if probe != -1 || width == 0 || height == 0 || byte_len == 0 {
        return Err("The rendered preview frame is no longer available".into());
    }
    let expected = width as usize * height as usize * 4;
    if byte_len != expected {
        return Err(format!("Preview frame size mismatch: expected {expected} bytes, got {byte_len}"));
    }
    let mut pixels = vec![0_u8; byte_len];
    let result = unsafe {
        hanabi_engine::hanabi_engine_copy_preview_frame(
            request_id,
            pixels.as_mut_ptr(),
            pixels.len(),
            &mut width,
            &mut height,
            &mut byte_len,
            &mut at,
        )
    };
    if result != 1 {
        return Err("Could not copy the rendered preview frame".into());
    }
    Ok((width, height, at, pixels))
}
