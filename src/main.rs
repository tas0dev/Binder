use swiftlib::{
    ipc::{ipc_recv, ipc_send},
    keyboard::{read_scancode, read_scancode_tap},
    privileged,
    task::{find_process_by_name, yield_now},
    vga,
};

const IPC_BUF_SIZE: usize = 4128;
const KAGAMI_PROCESS_CANDIDATES: [&str; 3] =
    ["/Applications/Kagami.app/entry.elf", "Kagami.app", "entry.elf"];

const OP_REQ_CREATE_WINDOW: u32 = 1;
const OP_RES_WINDOW_CREATED: u32 = 2;
const OP_REQ_FLUSH_CHUNK: u32 = 4;
const OP_REQ_ATTACH_SHARED: u32 = 5;
const OP_REQ_PRESENT_SHARED: u32 = 6;
const OP_RES_SHARED_ATTACHED: u32 = 7;
const LAYER_WALLPAPER: u8 = 0;

fn main() {
    println!("[Binder] start desktop mock");
    let kagami_tid = match parse_kagami_tid_from_args().or_else(find_kagami_tid) {
        Some(tid) => tid,
        None => {
            eprintln!("[Binder] Kagami not found");
            return;
        }
    };

    let (width, height) = desktop_window_size();
    let window_id = match create_app_window(kagami_tid, width, height) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("[Binder] create window failed: {}", e);
            return;
        }
    };
    let pixels = render_desktop(width as usize, height as usize);
    if let Err(e) = flush_window_shared(kagami_tid, window_id, width, height, &pixels) {
        eprintln!("[Binder] shared flush failed: {}, fallback to chunk", e);
        if let Err(e2) = flush_window_chunked(kagami_tid, window_id, width, height, &pixels) {
            eprintln!("[Binder] flush failed: {}", e2);
            return;
        }
    }
    println!("[Binder] desktop shown");

    loop {
        let sc_opt = match read_scancode_tap() {
            Ok(Some(sc)) => Some(sc),
            Ok(None) => read_scancode(),
            Err(_) => read_scancode(),
        };
        if let Some(sc) = sc_opt
            && (sc == 0x01 || sc == 0x81)
        {
            println!("[Binder] exit");
            return;
        }
        yield_now();
    }
}

fn create_app_window(kagami_tid: u64, width: u16, height: u16) -> Result<u32, &'static str> {
    let mut req = [0u8; 9];
    req[0..4].copy_from_slice(&OP_REQ_CREATE_WINDOW.to_le_bytes());
    req[4..6].copy_from_slice(&width.to_le_bytes());
    req[6..8].copy_from_slice(&height.to_le_bytes());
    req[8] = LAYER_WALLPAPER;
    if (ipc_send(kagami_tid, &req) as i64) < 0 {
        return Err("send create_window failed");
    }
    let mut recv = [0u8; IPC_BUF_SIZE];
    for _ in 0..256 {
        let (sender, len) = ipc_recv(&mut recv);
        if sender != kagami_tid || len < 8 {
            yield_now();
            continue;
        }
        let op = u32::from_le_bytes([recv[0], recv[1], recv[2], recv[3]]);
        if op != OP_RES_WINDOW_CREATED {
            continue;
        }
        return Ok(u32::from_le_bytes([recv[4], recv[5], recv[6], recv[7]]));
    }
    Err("window create timeout")
}

fn flush_window_shared(
    kagami_tid: u64,
    window_id: u32,
    width: u16,
    height: u16,
    pixels: &[u32],
) -> Result<(), &'static str> {
    let total = width as usize * height as usize;
    if pixels.len() < total {
        return Err("pixel buffer too small");
    }
    let total_bytes = total.checked_mul(4).ok_or("size overflow")?;
    let page_count = total_bytes.div_ceil(4096);
    if page_count == 0 {
        return Err("shared surface page count out of range");
    }

    let mut phys_pages = vec![0u64; page_count];
    let virt_addr = unsafe {
        privileged::alloc_shared_pages(page_count as u64, Some(phys_pages.as_mut_slice()), 0)
    };
    if (virt_addr as i64) < 0 || virt_addr == 0 {
        return Err("alloc_shared_pages failed");
    }

    unsafe {
        let dst = core::slice::from_raw_parts_mut(virt_addr as *mut u32, total);
        for (d, s) in dst.iter_mut().zip(pixels.iter()) {
            *d = *s | 0xFF00_0000;
        }
    }

    let mut attach = [0u8; 12];
    attach[0..4].copy_from_slice(&OP_REQ_ATTACH_SHARED.to_le_bytes());
    attach[4..8].copy_from_slice(&window_id.to_le_bytes());
    attach[8..10].copy_from_slice(&width.to_le_bytes());
    attach[10..12].copy_from_slice(&height.to_le_bytes());
    if (ipc_send(kagami_tid, &attach) as i64) < 0 {
        return Err("failed to send shared attach");
    }
    let send_pages_ret = unsafe { privileged::ipc_send_pages(kagami_tid, phys_pages.as_slice(), 0) };
    if (send_pages_ret as i64) < 0 {
        return Err("failed to send shared pages");
    }
    wait_shared_attach_ack(kagami_tid, window_id)?;

    let mut present = [0u8; 8];
    present[0..4].copy_from_slice(&OP_REQ_PRESENT_SHARED.to_le_bytes());
    present[4..8].copy_from_slice(&window_id.to_le_bytes());
    if (ipc_send(kagami_tid, &present) as i64) < 0 {
        return Err("failed to send shared present");
    }
    Ok(())
}

fn wait_shared_attach_ack(kagami_tid: u64, window_id: u32) -> Result<(), &'static str> {
    let mut recv = [0u8; IPC_BUF_SIZE];
    for _ in 0..256 {
        let (sender, len) = ipc_recv(&mut recv);
        if sender != kagami_tid || len < 8 {
            yield_now();
            continue;
        }
        let op = u32::from_le_bytes([recv[0], recv[1], recv[2], recv[3]]);
        if op != OP_RES_SHARED_ATTACHED {
            continue;
        }
        let ack_window = u32::from_le_bytes([recv[4], recv[5], recv[6], recv[7]]);
        if ack_window == window_id {
            return Ok(());
        }
    }
    Err("shared attach ack timeout")
}

fn flush_window_chunked(
    kagami_tid: u64,
    window_id: u32,
    width: u16,
    height: u16,
    pixels: &[u32],
) -> Result<(), &'static str> {
    let total = width as usize * height as usize;
    if pixels.len() < total {
        return Err("pixel buffer too small");
    }
    let chunk_header = 20usize;
    let max_chunk_pixels = (IPC_BUF_SIZE - chunk_header) / 4;
    let width_usize = width as usize;
    let height_usize = height as usize;
    let chunk_w = width_usize.min(96).max(1);
    let chunk_h = (max_chunk_pixels / chunk_w).max(1);

    let mut y0 = 0usize;
    while y0 < height_usize {
        let h = (height_usize - y0).min(chunk_h);
        let mut x0 = 0usize;
        while x0 < width_usize {
            let w = (width_usize - x0).min(chunk_w);
            let mut msg = vec![0u8; chunk_header + (w * h * 4)];
            msg[0..4].copy_from_slice(&OP_REQ_FLUSH_CHUNK.to_le_bytes());
            msg[4..8].copy_from_slice(&window_id.to_le_bytes());
            msg[8..10].copy_from_slice(&width.to_le_bytes());
            msg[10..12].copy_from_slice(&height.to_le_bytes());
            msg[12..14].copy_from_slice(&(x0 as u16).to_le_bytes());
            msg[14..16].copy_from_slice(&(y0 as u16).to_le_bytes());
            msg[16..18].copy_from_slice(&(w as u16).to_le_bytes());
            msg[18..20].copy_from_slice(&(h as u16).to_le_bytes());
            let mut off = chunk_header;
            for row in 0..h {
                let src_row = (y0 + row) * width_usize;
                for col in 0..w {
                    msg[off..off + 4]
                        .copy_from_slice(&(pixels[src_row + x0 + col] | 0xFF00_0000).to_le_bytes());
                    off += 4;
                }
            }
            if (ipc_send(kagami_tid, &msg) as i64) < 0 {
                return Err("send flush chunk failed");
            }
            x0 += w;
        }
        y0 += h;
    }
    Ok(())
}

fn render_desktop(width: usize, height: usize) -> Vec<u32> {
    let mut px = vec![0u32; width * height];
    for y in 0..height {
        let t = y as u32 * 255 / height as u32;
        let r = 220u32.saturating_sub(t / 3);
        let g = 232u32.saturating_sub(t / 5);
        let b = 244u32.saturating_sub(t / 7);
        let c = 0xFF00_0000 | (r << 16) | (g << 8) | b;
        let row = y * width;
        for x in 0..width {
            px[row + x] = c;
        }
    }

    fill_rect(&mut px, width, 0, 0, width as i32, 32, 0xFFF4_F7FA);
    draw_text(&mut px, width, 24, 10, "mochiOS", 0xFF47_5569);
    draw_text(
        &mut px,
        width,
        width as i32 - 86,
        10,
        "12:45 PM",
        0xFF47_5569,
    );

    let dock_w = 276i32;
    let dock_h = 75i32;
    let dock_x = (width as i32 - dock_w) / 2;
    let dock_y = (height as i32 - dock_h - 14).max(40);
    fill_rounded_rect(&mut px, width, dock_x, dock_y, dock_w, dock_h, 18, 0xEAF6_F8FC);
    stroke_rounded_rect(&mut px, width, dock_x, dock_y, dock_w, dock_h, 18, 0xFFCD_D7E4);

    let mut icon_x = dock_x + 18;
    let icon_y = dock_y + 18;
    let icons = [0xFF60_A5FA, 0xFF4ADE80, 0xFFF59E0B, 0xFFFB7185, 0xFFA78BFA];
    for c in icons {
        fill_rounded_rect(&mut px, width, icon_x, icon_y, 40, 40, 12, c);
        icon_x += 48;
    }

    px
}

fn fill_rect(px: &mut [u32], stride: usize, x: i32, y: i32, w: i32, h: i32, color: u32) {
    if w <= 0 || h <= 0 {
        return;
    }
    let hmax = px.len() / stride;
    let x0 = x.max(0) as usize;
    let y0 = y.max(0) as usize;
    let x1 = (x + w).max(0) as usize;
    let y1 = (y + h).max(0) as usize;
    let x1 = x1.min(stride);
    let y1 = y1.min(hmax);
    for yy in y0..y1 {
        let row = yy * stride;
        for xx in x0..x1 {
            px[row + xx] = color;
        }
    }
}

fn fill_rounded_rect(
    px: &mut [u32],
    stride: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    radius: i32,
    color: u32,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let r = radius.min(w / 2).min(h / 2).max(0);
    for yy in 0..h {
        for xx in 0..w {
            if inside_rounded_rect(xx, yy, w, h, r) {
                put(px, stride, x + xx, y + yy, color);
            }
        }
    }
}

fn stroke_rounded_rect(
    px: &mut [u32],
    stride: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    radius: i32,
    color: u32,
) {
    if w <= 2 || h <= 2 {
        return;
    }
    let r = radius.min(w / 2).min(h / 2).max(0);
    for yy in 0..h {
        for xx in 0..w {
            let outer = inside_rounded_rect(xx, yy, w, h, r);
            let inner = inside_rounded_rect(xx - 1, yy - 1, w - 2, h - 2, (r - 1).max(0));
            if outer && !inner {
                put(px, stride, x + xx, y + yy, color);
            }
        }
    }
}

fn inside_rounded_rect(xx: i32, yy: i32, w: i32, h: i32, r: i32) -> bool {
    if xx < 0 || yy < 0 || xx >= w || yy >= h {
        return false;
    }
    if r <= 0 || (xx >= r && xx < w - r) || (yy >= r && yy < h - r) {
        return true;
    }
    let cx = if xx < r { r - 1 } else { w - r };
    let cy = if yy < r { r - 1 } else { h - r };
    let dx = xx - cx;
    let dy = yy - cy;
    dx * dx + dy * dy <= r * r
}

fn draw_text(px: &mut [u32], stride: usize, x: i32, y: i32, text: &str, color: u32) {
    let mut pen_x = x;
    for ch in text.bytes() {
        draw_char(px, stride, pen_x, y, ch, color);
        pen_x += 6;
    }
}

fn draw_char(px: &mut [u32], stride: usize, x: i32, y: i32, ch: u8, color: u32) {
    let g = glyph(ch);
    for (row, bits) in g.iter().enumerate() {
        for col in 0..5 {
            if (bits >> (4 - col)) & 1 == 1 {
                put(px, stride, x + col as i32, y + row as i32, color);
            }
        }
    }
}

fn put(px: &mut [u32], stride: usize, x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 {
        return;
    }
    let x = x as usize;
    let y = y as usize;
    let h = px.len() / stride;
    if x >= stride || y >= h {
        return;
    }
    px[y * stride + x] = color;
}

fn glyph(ch: u8) -> [u8; 7] {
    match ch {
        b'm' => [0x00, 0x1A, 0x15, 0x15, 0x15, 0x15, 0x15],
        b'o' => [0x00, 0x0E, 0x11, 0x11, 0x11, 0x11, 0x0E],
        b'c' => [0x00, 0x0E, 0x11, 0x10, 0x10, 0x11, 0x0E],
        b'h' => [0x10, 0x10, 0x16, 0x19, 0x11, 0x11, 0x11],
        b'i' => [0x04, 0x00, 0x0C, 0x04, 0x04, 0x04, 0x0E],
        b's' => [0x00, 0x0F, 0x10, 0x0E, 0x01, 0x11, 0x0E],
        b'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        b'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        b':' => [0x00, 0x04, 0x04, 0x00, 0x04, 0x04, 0x00],
        b' ' => [0; 7],
        b'0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        b'1' => [0x04, 0x0C, 0x14, 0x04, 0x04, 0x04, 0x1F],
        b'2' => [0x0E, 0x11, 0x01, 0x06, 0x08, 0x10, 0x1F],
        b'4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        b'5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        _ => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x00, 0x08],
    }
}

fn find_kagami_tid() -> Option<u64> {
    for name in KAGAMI_PROCESS_CANDIDATES {
        if let Some(tid) = find_process_by_name(name) {
            return Some(tid);
        }
    }
    None
}

fn desktop_window_size() -> (u16, u16) {
    if let Some(info) = vga::get_info() {
        let w = info.width.clamp(1, u16::MAX as u32) as u16;
        let h = info.height.clamp(1, u16::MAX as u32) as u16;
        return (w, h);
    }
    (1280, 800)
}

fn parse_kagami_tid_from_args() -> Option<u64> {
    for arg in std::env::args().skip(1) {
        if let Some(rest) = arg.strip_prefix("--kagami-tid=")
            && let Ok(tid) = rest.parse::<u64>()
            && tid != 0
        {
            return Some(tid);
        }
    }
    None
}
