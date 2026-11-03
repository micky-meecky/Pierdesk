use std::process::Command;

#[test]
fn proxy_mode_does_not_enter_inherited_askpass_or_keygen_handlers() {
    for variable in [
        "PIERDESK_SSH_ASKPASS_ID",
        pierdesk::ssh::KEYGEN_SECRET_ENV,
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_pierdesk"))
            .args(["ssh-proxy", "invalid-id"])
            .env(variable, "nonexistent-test-value")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            output.stdout.is_empty(),
            "proxy stdout must contain only SSH bytes"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Invalid SSH connection identity")
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One isolated real-process IPC/stdio lifecycle.
fn proxy_preserves_binary_stdio_after_slow_setup_and_exits_on_input_close() {
    use std::fs::{self, File};
    use std::io::{Read, Write};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    use windows::Win32::{
        Foundation::ERROR_PIPE_CONNECTED,
        Storage::FileSystem::PIPE_ACCESS_DUPLEX,
        System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        },
    };
    let root = tempfile::tempdir().unwrap();
    let store = pierdesk::device_store::DeviceStore::new(root.path().join("ClipFerry"));
    let identity = store.load_or_create_identity().unwrap().identity;
    fs::create_dir_all(store.root().join("ssh")).unwrap();
    fs::write(
        store.root().join("ssh/proxy-runtime.json"),
        serde_json::to_vec(&serde_json::json!({
            "pid":std::process::id(),"executable":std::env::current_exe().unwrap()
        }))
        .unwrap(),
    )
    .unwrap();
    let name: Vec<u16> = format!(r"\\.\pipe\ClipFerry-SSH-{}", identity.fingerprint())
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // SAFETY: unique local test pipe; File assumes sole ownership of the live handle.
    let handle = unsafe {
        CreateNamedPipeW(
            windows_core::PCWSTR(name.as_ptr()),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            65536,
            65536,
            0,
            None,
        )
    };
    assert!(!handle.is_invalid());
    let mut pipe = unsafe { File::from_raw_handle(handle.0) };
    let server = std::thread::spawn(move || {
        let result = unsafe {
            ConnectNamedPipe(
                windows::Win32::Foundation::HANDLE(pipe.as_raw_handle()),
                None,
            )
        };
        assert!(
            result.is_ok()
                || result.unwrap_err().code()
                    == windows_core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0)
        );
        let mut size = [0; 4];
        pipe.read_exact(&mut size).unwrap();
        let mut request = vec![0; u32::from_be_bytes(size) as usize];
        pipe.read_exact(&mut request).unwrap();
        let request: serde_json::Value = serde_json::from_slice(&request).unwrap();
        assert_eq!(request["action"], "tunnel");
        std::thread::sleep(Duration::from_secs(6));
        let response = br#"{"ok":true}"#;
        pipe.write_all(&u32::try_from(response.len()).unwrap().to_be_bytes())
            .unwrap();
        pipe.write_all(response).unwrap();
        pipe.write_all(&[0, 255, 128, 1, 3]).unwrap();
        let mut input = [0; 5];
        pipe.read_exact(&mut input).unwrap();
        assert_eq!(input, [9, 0, 255, 128, 8]);
        pipe.write_all(&input).unwrap();
        let result = pipe.read(&mut [0]);
        assert!(
            result.is_err() || result.unwrap() == 0,
            "stdin close must close the proxy pipe"
        );
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_pierdesk"))
        .args(["ssh-proxy", "00000000000000000000000000000001"])
        .env("LOCALAPPDATA", root.path())
        .env("PIERDESK_SSH_ASKPASS_ID", "inherited-unrelated-id")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    input.write_all(&[9, 0, 255, 128, 8]).unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut bytes = [0; 10];
        let result = output.read_exact(&mut bytes);
        let _ = sender.send(result.map(|()| bytes));
    });
    let result = receiver.recv_timeout(Duration::from_secs(15));
    if result.is_err() {
        let _ = child.kill();
    }
    assert_eq!(
        result.unwrap().unwrap(),
        [0, 255, 128, 1, 3, 9, 0, 255, 128, 8]
    );
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("proxy did not exit after stdin closed");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    reader.join().unwrap();
    server.join().unwrap();
}
