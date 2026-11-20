//! 截图 OCR，用 Vision 框架。
//!
//! 以前这活是 `scripts/ocr.swift`：一个 20 行的 Swift 脚本 + 每次 swiftc 编译。
//! 仓库的语言约束是 Rust + Go，所以用 `objc2-vision`（开源绑定）重写——
//! **没有自己写 FFI 声明**，都是库提供的安全封装。
//!
//! 用途只有一个：界面探针要判断"屏幕上到底渲染出了什么"，
//! 靠 OCR 而不是靠人眼（人眼验证过一次就过期，OCR 每次都能重跑）。
use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_foundation::{NSArray, NSString, NSURL};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
};

pub fn recognize(path: &str) -> Result<Vec<String>, String> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    let handler = unsafe {
        VNImageRequestHandler::initWithURL_options(
            VNImageRequestHandler::alloc(),
            &url,
            &objc2_foundation::NSDictionary::new(),
        )
    };

    let request = unsafe { VNRecognizeTextRequest::new() };
    unsafe {
        request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        let langs = NSArray::from_retained_slice(&[
            NSString::from_str("zh-Hans"),
            NSString::from_str("en-US"),
        ]);
        request.setRecognitionLanguages(&langs);
    }

    let reqs: Retained<NSArray<VNRequest>> = {
        let arr = NSArray::from_slice(&[&*request as &VNRequest]);
        unsafe { std::mem::transmute::<Retained<NSArray<VNRequest>>, Retained<NSArray<VNRequest>>>(arr) }
    };
    unsafe { handler.performRequests_error(&reqs) }.map_err(|e| format!("performRequests: {e}"))?;

    let mut out = Vec::new();
    if let Some(results) = unsafe { request.results() } {
        for obs in results.iter() {
            if let Some(top) = unsafe { obs.topCandidates(1) }.iter().next() {
                out.push(unsafe { top.string() }.to_string());
            }
        }
    }
    Ok(out)
}
