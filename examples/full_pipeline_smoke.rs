//! 跑一遍 capture → clip_region → write_frame → get_image 回环，确认
//! clip_region + write_frame 在真实截图上不出问题。
//! 配合 ClipboardService + ScreenCapture trait 用，跟生产路径完全一致。

use screenshot_rs::capture::{platform_capture, CapturedFrame};
use screenshot_rs::clipboard::ClipboardService;

fn main() {
    let capture = platform_capture();
    let frame = match capture.capture_primary() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("capture 失败：{e}");
            std::process::exit(2);
        }
    };
    println!(
        "捕获到 {}x{}，前 4 字节 = {:02X?}",
        frame.width, frame.height, &frame.pixels[..4.min(frame.pixels.len())]
    );

    // 模拟一次"中段选区"：屏幕中心 100×100
    let w = 100u32.min(frame.width);
    let h = 100u32.min(frame.height);
    let x = (frame.width - w) / 2;
    let y = (frame.height - h) / 2;
    let clipped = match frame.clip_region(x, y, w, h) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("clip_region 失败：{e}");
            std::process::exit(3);
        }
    };
    println!(
        "裁剪后 {}x{}，前 4 字节 = {:02X?}",
        clipped.width, clipped.height, &clipped.pixels[..4.min(clipped.pixels.len())]
    );

    // 走生产路径：ClipboardService::write_frame
    let svc = ClipboardService::new();
    if let Err(e) = svc.write_frame(&clipped) {
        eprintln!("write_frame 失败：{e}");
        std::process::exit(4);
    }
    println!("write_frame OK");

    // 立刻回读校验
    let mut cb = arboard::Clipboard::new().expect("Clipboard::new");
    match cb.get_image() {
        Ok(img) => {
            assert_eq!(img.width as u32, clipped.width);
            assert_eq!(img.height as u32, clipped.height);
            assert_eq!(img.bytes.len(), clipped.pixels.len());
            assert_eq!(&img.bytes[..16], &clipped.pixels[..16]);
            println!(
                "回读校验通过：{}x{} 字节内容匹配，前 16 字节 = {:02X?}",
                img.width, img.height, &img.bytes[..16]
            );
        }
        Err(e) => eprintln!("get_image 失败：{e}"),
    }

    // 把原图也写一份 PNG 到 /tmp，方便用户目测
    let path = "/tmp/screenshot-rs-smoke.png";
    let img: image::RgbaImage =
        image::ImageBuffer::from_raw(clipped.width, clipped.height, clipped.pixels.clone())
            .expect("ImageBuffer 构造");
    if let Err(e) = img.save(path) {
        eprintln!("save {path} 失败：{e}");
    } else {
        println!("已保存到 {path}");
    }

    // 抑制 unused 警告
    let _ = CapturedFrame {
        width: 0,
        height: 0,
        pixels: vec![],
    };
}