//! Responses 分段计时；仅传递枚举、计数和耗时，不保留请求内容。

use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

/// Responses 请求阶段及原生投影子阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesDiagnosticStage {
    /// 请求准备中的压缩点选择与图片预算子阶段。
    CompactionProjection,
    /// 包含会话转换、投影、认证准备和请求体构建。
    RequestPreparation,
    /// 从发起 HTTP 请求到收到响应头，包含请求体上传。
    HttpHeaders,
    /// 从响应头到终止事件到达；不包含终止响应解码及检查点签发。
    StreamReceive,
    /// 终止响应解码、兼容处理和 finish_response（含检查点）。
    TerminalDecode,
}

/// 可写入统一日志的安全诊断字段；不接受任意文本或 JSON 内容。
#[derive(Debug, Clone, Serialize)]
pub struct ResponsesDiagnostics {
    /// 当前计时阶段。
    pub stage: ResponsesDiagnosticStage,
    /// 是否为 SSE 请求；非流式使用相同阶段记录响应体接收。
    pub streaming: bool,
    /// 阶段耗时，单位为毫秒。
    pub elapsed_ms: f64,
    /// 阶段是否正常完成；错误返回或提前丢弃时为 false。
    pub completed: bool,
    /// 最终 HTTP 请求 input 数组的条目数，字符串输入视为一项。
    pub input_items: Option<u64>,
    /// 最终 HTTP 请求体字节数；启用传输压缩时为压缩后大小。
    pub request_body_bytes: Option<u64>,
    /// 已收到的 HTTP 状态码。
    pub status_code: Option<u16>,
    /// 仅原生压缩投影子阶段提供。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<ResponsesCompactionDiagnostics>,
}

/// 压缩投影仅记录固定原因及规模，避免输出历史或加密内容。
#[derive(Debug, Clone, Serialize)]
pub struct ResponsesCompactionDiagnostics {
    pub history_items: usize,
    pub selected: bool,
    pub candidates: usize,
    pub rejections: std::collections::BTreeMap<String, usize>,
    pub body_bytes: usize,
    pub images_evicted: usize,
}

/// 调用方可将诊断转成事件；未设置时不发送诊断。
pub type ResponsesDiagnosticsObserver = Arc<dyn Fn(ResponsesDiagnostics) + Send + Sync>;

/// 离开阶段时立即发送，确保初始化失败及流被丢弃也能留下已有耗时。
pub(super) struct StageTimer {
    observer: Option<ResponsesDiagnosticsObserver>,
    started: Instant,
    data: ResponsesDiagnostics,
    emitted: bool,
}

impl StageTimer {
    /// 开始一个阶段；计时对象不持有请求体和认证信息。
    pub(super) fn new(
        observer: Option<ResponsesDiagnosticsObserver>,
        streaming: bool,
        stage: ResponsesDiagnosticStage,
    ) -> Self {
        Self::starting_at(observer, streaming, stage, Instant::now())
    }

    /// 使用事件到达时刻，避免把解析开销计入流接收。
    pub(super) fn starting_at(
        observer: Option<ResponsesDiagnosticsObserver>,
        streaming: bool,
        stage: ResponsesDiagnosticStage,
        started: Instant,
    ) -> Self {
        Self {
            observer,
            started,
            data: ResponsesDiagnostics {
                stage,
                streaming,
                elapsed_ms: 0.0,
                completed: false,
                input_items: None,
                request_body_bytes: None,
                status_code: None,
                compaction: None,
            },
            emitted: false,
        }
    }

    /// 从最终投影及已序列化的 HTTP 请求读取规模，不额外序列化或克隆正文。
    pub(super) fn record_request(
        &mut self,
        input: Option<&serde_json::Value>,
        request: &reqwest::Request,
    ) {
        self.data.input_items = input.and_then(|input| match input {
            serde_json::Value::Array(items) => Some(items.len() as u64),
            serde_json::Value::String(_) => Some(1),
            _ => None,
        });
        self.data.request_body_bytes = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .map(|bytes| bytes.len() as u64);
    }

    /// 只记录数值状态码。
    pub(super) fn record_status(&mut self, status: reqwest::StatusCode) {
        self.data.status_code = Some(status.as_u16());
    }

    /// 完成当前阶段并传递安全规模字段到下一阶段。
    pub(super) fn next(mut self, stage: ResponsesDiagnosticStage) -> Self {
        let now = Instant::now();
        self.emit(true, now);
        let mut next = Self::starting_at(self.observer.clone(), self.data.streaming, stage, now);
        next.data.input_items = self.data.input_items;
        next.data.request_body_bytes = self.data.request_body_bytes;
        next.data.status_code = self.data.status_code;
        next
    }

    /// 结束阶段；失败不携带可能含有上游正文的错误文本。
    pub(super) fn finish(mut self, completed: bool) {
        self.emit(completed, Instant::now());
    }

    /// 在终止事件刚到达时结束接收阶段，并开始解码计时。
    pub(super) fn terminal(mut self, arrived: Instant) -> Self {
        self.emit(true, arrived);
        let mut next = Self::starting_at(
            self.observer.clone(),
            self.data.streaming,
            ResponsesDiagnosticStage::TerminalDecode,
            arrived,
        );
        next.data.input_items = self.data.input_items;
        next.data.request_body_bytes = self.data.request_body_bytes;
        next.data.status_code = self.data.status_code;
        next
    }

    /// 每个阶段最多发送一次，Drop 只补发未完成阶段。
    fn emit(&mut self, completed: bool, ended: Instant) {
        if self.emitted {
            return;
        }
        self.emitted = true;
        self.data.elapsed_ms = ended.saturating_duration_since(self.started).as_secs_f64() * 1000.0;
        self.data.completed = completed;
        if let Some(observer) = &self.observer {
            observer(self.data.clone());
        }
    }
}

impl Drop for StageTimer {
    /// 提前返回和取消均保留当前未完成阶段。
    fn drop(&mut self) {
        self.emit(false, Instant::now());
    }
}
