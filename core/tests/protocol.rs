use filesearch_core::{
    filesearch_engine_call, filesearch_engine_close, filesearch_engine_free_string,
    filesearch_engine_open,
};
use serde_json::{json, Value};
use std::{
    ffi::{CStr, CString},
    ptr,
};

fn invoke(handle: *mut std::ffi::c_void, request: &str) -> Value {
    let request = CString::new(request).unwrap();
    // The test owns the live handle and keeps its NUL-terminated input alive.
    let response = unsafe { filesearch_engine_call(handle, request.as_ptr()) };
    assert!(!response.is_null());
    let value = unsafe { serde_json::from_slice(CStr::from_ptr(response).to_bytes()).unwrap() };
    unsafe { filesearch_engine_free_string(response) };
    value
}

#[test]
fn version_two_c_abi_uses_the_same_explicit_envelope_for_every_result() {
    let fixture = tempfile::tempdir().unwrap();
    let path = CString::new(fixture.path().join("index.sqlite").to_str().unwrap()).unwrap();
    let handle = unsafe { filesearch_engine_open(path.as_ptr()) };
    assert!(!handle.is_null());
    let success = invoke(handle, &json!({"op":"status"}).to_string());
    let unknown = invoke(
        handle,
        &json!({"op":"not-a-supported-operation"}).to_string(),
    );
    let malformed = invoke(handle, "{invalid json");
    let absent = invoke(ptr::null_mut(), "{}");
    unsafe { filesearch_engine_close(handle) };
    assert_eq!(success["success"], true);
    for response in [&unknown, &malformed, &absent] {
        assert_eq!(response["success"], false);
        assert!(response["error"].is_string());
    }
    for response in [&success, &unknown, &malformed, &absent] {
        assert_eq!(response["protocol_version"], 2);
        assert!(response.get("ok").is_none());
    }
}
