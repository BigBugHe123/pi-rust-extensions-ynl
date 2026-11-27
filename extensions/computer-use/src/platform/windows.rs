//! Native Windows desktop operations. This module calls Win32 directly so the
//! extension remains a single native executable with no script/helper install.
#![allow(unsafe_code)]

use std::path::Path;

use pi_ext_sdk::{Error, Result};
use serde_json::{Value, json};

use crate::outline::Rect;
use crate::state::{Root, Target};

#[cfg(windows)]
mod native {
    use super::*;
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::ptr::null_mut;

    type Hwnd = *mut c_void;
    type Hdc = *mut c_void;
    type Hbitmap = *mut c_void;
    type Hgdobj = *mut c_void;

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const SRCCOPY: u32 = 0x00CC0020;
    const DIB_RGB_COLORS: u32 = 0;
    const BI_RGB: u32 = 0;
    const INPUT_MOUSE: u32 = 0;
    const INPUT_KEYBOARD: u32 = 1;
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    const KEYEVENTF_UNICODE: u32 = 0x0004;
    const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
    const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
    const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
    const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
    const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
    const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
    const MOUSEEVENTF_WHEEL: u32 = 0x0800;
    const MOUSEEVENTF_HWHEEL: u32 = 0x1000;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct WinRect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[repr(C)]
    struct BitmapInfoHeader {
        size: u32,
        width: i32,
        height: i32,
        planes: u16,
        bit_count: u16,
        compression: u32,
        size_image: u32,
        x_pels_per_meter: i32,
        y_pels_per_meter: i32,
        clr_used: u32,
        clr_important: u32,
    }
    #[repr(C)]
    struct BitmapInfo {
        header: BitmapInfoHeader,
        colors: [u32; 1],
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct MouseInput {
        dx: i32,
        dy: i32,
        mouse_data: u32,
        flags: u32,
        time: u32,
        extra: usize,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct KeyboardInput {
        vk: u16,
        scan: u16,
        flags: u32,
        time: u32,
        extra: usize,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    union InputUnion {
        mouse: MouseInput,
        keyboard: KeyboardInput,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Input {
        kind: u32,
        data: InputUnion,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumWindows(
            callback: Option<unsafe extern "system" fn(Hwnd, isize) -> i32>,
            data: isize,
        ) -> i32;
        fn IsWindowVisible(hwnd: Hwnd) -> i32;
        fn IsIconic(hwnd: Hwnd) -> i32;
        fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max: i32) -> i32;
        fn GetWindowRect(hwnd: Hwnd, rect: *mut WinRect) -> i32;
        fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
        fn GetForegroundWindow() -> Hwnd;
        fn SetForegroundWindow(hwnd: Hwnd) -> i32;
        fn SetCursorPos(x: i32, y: i32) -> i32;
        fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
        fn SetProcessDPIAware() -> i32;
        fn GetSystemMetrics(index: i32) -> i32;
        fn GetDC(hwnd: Hwnd) -> Hdc;
        fn ReleaseDC(hwnd: Hwnd, dc: Hdc) -> i32;
    }
    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn CreateCompatibleDC(dc: Hdc) -> Hdc;
        fn CreateCompatibleBitmap(dc: Hdc, width: i32, height: i32) -> Hbitmap;
        fn SelectObject(dc: Hdc, object: Hgdobj) -> Hgdobj;
        fn BitBlt(
            dst: Hdc,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            src: Hdc,
            sx: i32,
            sy: i32,
            rop: u32,
        ) -> i32;
        fn GetDIBits(
            dc: Hdc,
            bitmap: Hbitmap,
            start: u32,
            lines: u32,
            bits: *mut c_void,
            info: *mut BitmapInfo,
            usage: u32,
        ) -> i32;
        fn DeleteObject(object: Hgdobj) -> i32;
        fn DeleteDC(dc: Hdc) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn QueryFullProcessImageNameW(
            process: *mut c_void,
            flags: u32,
            name: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    #[derive(Clone)]
    struct Window {
        hwnd: isize,
        pid: i64,
        title: String,
        app: String,
        rect: WinRect,
        minimized: bool,
        focused: bool,
    }

    fn text(value: &[u16]) -> String {
        String::from_utf16_lossy(value)
            .trim_end_matches('\0')
            .to_owned()
    }

    fn process_name(pid: u32) -> String {
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return "Unknown App".into();
            }
            let mut path = vec![0u16; 32768];
            let mut len = path.len() as u32;
            let ok = QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut len);
            let _ = CloseHandle(process);
            if ok == 0 {
                return "Unknown App".into();
            }
            Path::new(&text(&path[..len as usize]))
                .file_stem()
                .and_then(|v| v.to_str())
                .unwrap_or("Unknown App")
                .to_owned()
        }
    }

    unsafe extern "system" fn collect(hwnd: Hwnd, data: isize) -> i32 {
        let windows = unsafe { &mut *(data as *mut Vec<Window>) };
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        let mut rect = WinRect::default();
        if unsafe { GetWindowRect(hwnd, &mut rect) } == 0
            || rect.right <= rect.left
            || rect.bottom <= rect.top
        {
            return 1;
        }
        let mut title = vec![0u16; 2048];
        let len = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
        let title = text(&title[..len.max(0) as usize]);
        if title.is_empty() {
            return 1;
        }
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        windows.push(Window {
            hwnd: hwnd as isize,
            pid: i64::from(pid),
            title,
            app: process_name(pid),
            rect,
            minimized: unsafe { IsIconic(hwnd) } != 0,
            focused: hwnd == unsafe { GetForegroundWindow() },
        });
        1
    }

    fn enumerate() -> Result<Vec<Window>> {
        unsafe {
            SetProcessDPIAware();
        }
        let mut windows = Vec::new();
        let ok = unsafe { EnumWindows(Some(collect), &mut windows as *mut _ as isize) };
        if ok == 0 {
            return Err(Error::msg("Windows could not enumerate desktop windows."));
        }
        Ok(windows)
    }

    pub fn roots() -> Result<Vec<Root>> {
        let max_x = unsafe { GetSystemMetrics(0) };
        let max_y = unsafe { GetSystemMetrics(1) };
        Ok(enumerate()?
            .into_iter()
            .map(|w| {
                let r = w.rect;
                let onscreen = r.right > 0 && r.bottom > 0 && r.left < max_x && r.top < max_y;
                Root {
                    app: w.app,
                    pid: w.pid,
                    window_title: w.title,
                    window_id: w.hwnd as i64,
                    frame_points: Rect {
                        x: f64::from(r.left),
                        y: f64::from(r.top),
                        w: f64::from(r.right - r.left),
                        h: f64::from(r.bottom - r.top),
                    },
                    scale_factor: 1.0,
                    is_minimized: w.minimized,
                    is_onscreen: onscreen,
                    is_focused: w.focused,
                    ..Root::default()
                }
            })
            .collect())
    }

    pub fn capture(target: &Target, path: &Path) -> Result<()> {
        unsafe {
            SetProcessDPIAware();
        }
        let Some(window) = enumerate()?
            .into_iter()
            .find(|w| w.hwnd == target.window_id as isize && w.pid == target.pid)
        else {
            return Err(Error::msg(
                "The selected Windows window is no longer available.",
            ));
        };
        let r = window.rect;
        let (width, height) = (r.right - r.left, r.bottom - r.top);
        if width < 2 || height < 2 {
            return Err(Error::msg(
                "The selected Windows window has no visible area.",
            ));
        }
        let (screen, mem, bitmap) = unsafe {
            let screen = GetDC(null_mut());
            let mem = if screen.is_null() {
                null_mut()
            } else {
                CreateCompatibleDC(screen)
            };
            let bitmap = if mem.is_null() {
                null_mut()
            } else {
                CreateCompatibleBitmap(screen, width, height)
            };
            (screen, mem, bitmap)
        };
        if screen.is_null() || mem.is_null() || bitmap.is_null() {
            unsafe {
                if !bitmap.is_null() {
                    let _ = DeleteObject(bitmap);
                };
                if !mem.is_null() {
                    let _ = DeleteDC(mem);
                };
                if !screen.is_null() {
                    let _ = ReleaseDC(null_mut(), screen);
                };
            }
            return Err(Error::msg(
                "Could not allocate a Windows screenshot buffer.",
            ));
        }
        let old = unsafe { SelectObject(mem, bitmap) };
        let copied = unsafe { BitBlt(mem, 0, 0, width, height, screen, r.left, r.top, SRCCOPY) };
        let mut info = BitmapInfo {
            header: BitmapInfoHeader {
                size: size_of::<BitmapInfoHeader>() as u32,
                width,
                height: -height,
                planes: 1,
                bit_count: 32,
                compression: BI_RGB,
                size_image: 0,
                x_pels_per_meter: 0,
                y_pels_per_meter: 0,
                clr_used: 0,
                clr_important: 0,
            },
            colors: [0],
        };
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        unsafe {
            SelectObject(mem, old);
        }
        let rows = if copied != 0 {
            unsafe {
                GetDIBits(
                    mem,
                    bitmap,
                    0,
                    height as u32,
                    bgra.as_mut_ptr().cast(),
                    &mut info,
                    DIB_RGB_COLORS,
                )
            }
        } else {
            0
        };
        unsafe {
            DeleteObject(bitmap);
            DeleteDC(mem);
            ReleaseDC(null_mut(), screen);
        }
        if rows == 0 {
            return Err(Error::msg("Windows could not capture the selected window."));
        }
        let max_dim = 1280u32;
        let scale = (max_dim as f64 / f64::from(width.max(height))).min(1.0);
        let out_w = (f64::from(width) * scale).round().max(1.0) as u32;
        let out_h = (f64::from(height) * scale).round().max(1.0) as u32;
        let mut rgb = Vec::with_capacity(out_w as usize * out_h as usize * 3);
        for oy in 0..out_h {
            let sy = ((oy as f64 / scale).floor() as u32).min(height as u32 - 1);
            for ox in 0..out_w {
                let sx = ((ox as f64 / scale).floor() as u32).min(width as u32 - 1);
                let i = (sy as usize * width as usize + sx as usize) * 4;
                rgb.extend_from_slice(&[bgra[i + 2], bgra[i + 1], bgra[i]]);
            }
        }
        let mut jpeg = Vec::new();
        jpeg_encoder::Encoder::new(&mut jpeg, 70)
            .encode(
                &rgb,
                out_w as u16,
                out_h as u16,
                jpeg_encoder::ColorType::Rgb,
            )
            .map_err(|e| Error::msg(format!("Could not encode Windows screenshot: {e}")))?;
        std::fs::write(path, jpeg)?;
        Ok(())
    }

    fn mouse(flags: u32, data: u32) -> Input {
        Input {
            kind: INPUT_MOUSE,
            data: InputUnion {
                mouse: MouseInput {
                    dx: 0,
                    dy: 0,
                    mouse_data: data,
                    flags,
                    time: 0,
                    extra: 0,
                },
            },
        }
    }
    fn key(vk: u16, flags: u32) -> Input {
        Input {
            kind: INPUT_KEYBOARD,
            data: InputUnion {
                keyboard: KeyboardInput {
                    vk,
                    scan: 0,
                    flags,
                    time: 0,
                    extra: 0,
                },
            },
        }
    }
    fn send(input: &[Input]) -> Result<()> {
        let sent = unsafe {
            SendInput(
                input.len() as u32,
                input.as_ptr(),
                size_of::<Input>() as i32,
            )
        };
        if sent != input.len() as u32 {
            return Err(Error::msg(
                "Windows rejected desktop input. The target may be running with higher privileges.",
            ));
        }
        Ok(())
    }
    fn move_to(x: f64, y: f64) -> Result<()> {
        if unsafe { SetCursorPos(x.round() as i32, y.round() as i32) } == 0 {
            Err(Error::msg("Windows could not move the pointer."))
        } else {
            Ok(())
        }
    }
    fn vk(name: &str) -> u16 {
        match name.to_lowercase().as_str() {
            "ctrl" | "control" => 0x11,
            "alt" | "option" | "opt" => 0x12,
            "shift" => 0x10,
            "cmd" | "command" | "meta" | "super" => 0x5B,
            "enter" | "return" => 0x0D,
            "tab" => 0x09,
            "escape" | "esc" => 0x1B,
            "backspace" => 0x08,
            "delete" | "forwarddelete" => 0x2E,
            "space" => 0x20,
            "up" | "arrowup" => 0x26,
            "down" | "arrowdown" => 0x28,
            "left" | "arrowleft" => 0x25,
            "right" | "arrowright" => 0x27,
            "home" => 0x24,
            "end" => 0x23,
            "pageup" => 0x21,
            "pagedown" => 0x22,
            value if value.len() == 1 => value.as_bytes()[0].to_ascii_uppercase() as u16,
            value if value.starts_with('f') => value[1..]
                .parse::<u16>()
                .ok()
                .filter(|n| (1..=24).contains(n))
                .map(|n| 0x6F + n)
                .unwrap_or(0),
            _ => 0,
        }
    }
    fn type_text(s: &str) -> Result<()> {
        let mut inputs = Vec::with_capacity(s.encode_utf16().count() * 2);
        for unit in s.encode_utf16() {
            inputs.push(key(unit, KEYEVENTF_UNICODE));
            inputs.push(key(unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        }
        if inputs.is_empty() {
            Ok(())
        } else {
            send(&inputs)
        }
    }
    fn button_flags(name: &str) -> (u32, u32) {
        match name {
            "right" => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            "middle" => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
            _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        }
    }

    pub fn act(target: &Target, steps: &[crate::platform::Step]) -> Vec<Value> {
        let hwnd = target.window_id as Hwnd;
        let mut results = Vec::new();
        for (index, step) in steps.iter().enumerate() {
            let result: Result<()> = (|| {
                match step {
                    crate::platform::Step::Press { .. }
                    | crate::platform::Step::SetText { .. }
                    | crate::platform::Step::Focus { .. } => {
                        return Err(Error::msg(
                            "Windows has no accessibility tree in this backend; use coordinate actions.",
                        ));
                    }
                    crate::platform::Step::Activate => {
                        if unsafe { SetForegroundWindow(hwnd) } == 0 {
                            return Err(Error::msg(
                                "Windows could not activate the selected window.",
                            ));
                        }
                    }
                    crate::platform::Step::Release => {
                        send(&[
                            mouse(MOUSEEVENTF_LEFTUP, 0),
                            key(0x5B, KEYEVENTF_KEYUP),
                            key(0x11, KEYEVENTF_KEYUP),
                            key(0x12, KEYEVENTF_KEYUP),
                            key(0x10, KEYEVENTF_KEYUP),
                        ])?;
                    }
                    crate::platform::Step::Move { x, y } => move_to(*x, *y)?,
                    crate::platform::Step::Click {
                        x,
                        y,
                        button,
                        count,
                    } => {
                        move_to(*x, *y)?;
                        let (down, up) = button_flags(button);
                        for _ in 0..(*count).max(1) {
                            send(&[mouse(down, 0), mouse(up, 0)])?;
                        }
                    }
                    crate::platform::Step::Drag { path } => {
                        if let Some((x, y)) = path.first() {
                            move_to(*x, *y)?;
                            send(&[mouse(MOUSEEVENTF_LEFTDOWN, 0)])?;
                            for (x, y) in path.iter().skip(1) {
                                move_to(*x, *y)?;
                            }
                            send(&[mouse(MOUSEEVENTF_LEFTUP, 0)])?;
                        }
                    }
                    crate::platform::Step::Scroll { x, y, dx, dy } => {
                        move_to(*x, *y)?;
                        let mut inputs = Vec::new();
                        if *dy != 0.0 {
                            inputs.push(mouse(
                                MOUSEEVENTF_WHEEL,
                                ((-dy / 40.0 * 120.0).round() as i32) as u32,
                            ));
                        }
                        if *dx != 0.0 {
                            inputs.push(mouse(
                                MOUSEEVENTF_HWHEEL,
                                ((dx / 40.0 * 120.0).round() as i32) as u32,
                            ));
                        }
                        if !inputs.is_empty() {
                            send(&inputs)?;
                        }
                    }
                    crate::platform::Step::Type { text } => type_text(text)?,
                    crate::platform::Step::Keys { keys } => {
                        let codes: Vec<u16> = keys.iter().map(|s| vk(s)).collect();
                        if codes.contains(&0) {
                            return Err(Error::msg(
                                "Windows does not recognize one of the requested keys.",
                            ));
                        }
                        let is_modifier = |v| matches!(v, 0x10 | 0x11 | 0x12 | 0x5B);
                        let chord = codes.len() > 1
                            && codes[..codes.len() - 1].iter().all(|v| is_modifier(*v));
                        let mut inputs = Vec::new();
                        if chord {
                            let modifiers = &codes[..codes.len() - 1];
                            inputs.extend(modifiers.iter().map(|v| key(*v, 0)));
                            let last = codes[codes.len() - 1];
                            inputs.push(key(last, 0));
                            inputs.push(key(last, KEYEVENTF_KEYUP));
                            inputs.extend(modifiers.iter().rev().map(|v| key(*v, KEYEVENTF_KEYUP)));
                        } else {
                            for code in codes {
                                inputs.push(key(code, 0));
                                inputs.push(key(code, KEYEVENTF_KEYUP));
                            }
                        }
                        if !inputs.is_empty() {
                            send(&inputs)?;
                        }
                    }
                }
                Ok(())
            })();
            match result {
                Ok(()) => results.push(json!({"index":index,"outcome":"worked"})),
                Err(e) => {
                    results.push(json!({"index":index,"outcome":"didnt","error":e.message()}));
                    break;
                }
            }
        }
        results
    }
}

#[cfg(windows)]
pub fn windows() -> Result<Vec<Root>> {
    native::roots()
}
#[cfg(not(windows))]
pub fn windows() -> Result<Vec<Root>> {
    Err(Error::msg(
        "The Windows backend is available only on Windows.",
    ))
}

#[cfg(windows)]
pub fn capture(target: &Target, path: &Path) -> Result<()> {
    native::capture(target, path)
}
#[cfg(not(windows))]
pub fn capture(_: &Target, _: &Path) -> Result<()> {
    Err(Error::msg(
        "The Windows backend is available only on Windows.",
    ))
}

#[cfg(windows)]
pub fn act(target: &Target, steps: &[crate::platform::Step]) -> Vec<Value> {
    native::act(target, steps)
}
#[cfg(not(windows))]
pub fn act(_: &Target, _: &[crate::platform::Step]) -> Vec<Value> {
    vec![]
}

#[cfg(windows)]
pub fn release() {
    let _ = native::act(&Target::default(), &[crate::platform::Step::Release]);
}
#[cfg(not(windows))]
pub fn release() {}
