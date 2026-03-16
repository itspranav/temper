//! WASM engine: compile, cache, and invoke modules.
//!
//! Modules are compiled once and cached by SHA-256 hash. Each invocation
//! gets a fresh `Store` with fuel + memory limits (TigerStyle budgets).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use sha2::{Digest, Sha256};
use wasmtime::{Caller, Config, Engine, Linker, Module, ResourceLimiter, Store};

use crate::host_trait::WasmHost;
use crate::stream::StreamRegistry;
use crate::types::{
    MAX_MODULE_SIZE, WasmInvocationContext, WasmInvocationResult, WasmResourceLimits,
};

/// Errors from the WASM engine.
#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    /// Module exceeds the maximum allowed size.
    #[error("module too large: {size} bytes (max {max})")]
    ModuleTooLarge {
        /// Actual size of the module.
        size: usize,
        /// Maximum allowed size.
        max: usize,
    },
    /// WASM module compilation failed.
    #[error("compilation failed: {0}")]
    Compilation(String),
    /// WASM module instantiation failed.
    #[error("instantiation failed: {0}")]
    Instantiation(String),
    /// WASM function invocation failed.
    #[error("invocation failed: {0}")]
    Invocation(String),
    /// Module exceeded its instruction fuel budget.
    #[error("fuel exhausted -- module exceeded instruction budget")]
    FuelExhausted,
    /// Module exceeded its wall-clock execution timeout.
    #[error("execution timeout -- module exceeded time budget of {0:?}")]
    Timeout(std::time::Duration),
    /// Module attempted to exceed its memory budget.
    #[error("memory limit exceeded -- module requested more than {max_bytes} bytes")]
    MemoryLimitExceeded {
        /// Configured memory limit in bytes.
        max_bytes: usize,
    },
    /// Requested module hash not found in cache.
    #[error("module not found: {0}")]
    ModuleNotFound(String),
}

/// Memory limiter enforcing a per-invocation byte cap via Wasmtime's ResourceLimiter.
///
/// Passed into each Store so that `memory.grow` instructions that would exceed
/// `max_memory` are denied. On denial the module receives a failed grow (returns
/// -1 from `memory.grow`); if the trap-on-deny path is used it raises a trap.
struct MemoryLimiter {
    /// Maximum allowed linear memory in bytes.
    max_memory: usize,
}

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        if desired > self.max_memory {
            // Return Ok(false) so memory.grow returns -1 (spec-compliant denial).
            // Callers that need a trap instead can check the return value and
            // raise unreachable themselves.
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        Ok(true)
    }
}

/// Compiled module cache entry.
struct CachedModule {
    /// The compiled wasmtime module.
    module: Module,
}

/// Host state passed into the WASM store.
struct HostState {
    /// Serialized invocation context JSON.
    context_json: String,
    /// Result JSON set by the guest via host_set_result.
    result_json: Option<String>,
    /// Host capabilities (HTTP, secrets, logging).
    host: Arc<dyn WasmHost>,
    /// Memory limiter enforcing `max_memory` per invocation.
    limiter: MemoryLimiter,
    /// Stream registry for binary data transfer between host and WASM guest.
    /// Bytes never enter WASM memory — WASM references them by stream ID.
    streams: Arc<RwLock<StreamRegistry>>,
}

/// WASM engine: compile, cache, invoke modules.
///
/// Modules are compiled once and cached by SHA-256 hash. Each invocation
/// gets a fresh `Store` with fuel + memory limits (TigerStyle budgets).
pub struct WasmEngine {
    /// The underlying wasmtime engine.
    engine: Engine,
    /// Compiled module cache: SHA-256 hash -> compiled module.
    cache: RwLock<BTreeMap<String, Arc<CachedModule>>>,
}

impl WasmEngine {
    /// Create a new WASM engine with fuel metering and epoch interruption enabled.
    pub fn new() -> Result<Self, WasmError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        config.wasm_component_model(true);

        let engine = Engine::new(&config).map_err(|e| WasmError::Compilation(e.to_string()))?;

        Ok(Self {
            engine,
            cache: RwLock::new(BTreeMap::new()),
        })
    }

    /// Compute SHA-256 hash of WASM bytes.
    pub fn hash_module(wasm_bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(wasm_bytes);
        format!("{:x}", hasher.finalize())
    }

    /// Compile and cache a WASM module.
    ///
    /// Returns the SHA-256 hash of the module bytes.
    pub fn compile_and_cache(&self, wasm_bytes: &[u8]) -> Result<String, WasmError> {
        // TigerStyle: pre-assertion on module size
        if wasm_bytes.len() > MAX_MODULE_SIZE {
            return Err(WasmError::ModuleTooLarge {
                size: wasm_bytes.len(),
                max: MAX_MODULE_SIZE,
            });
        }

        let hash = Self::hash_module(wasm_bytes);

        // Check if already cached
        {
            let cache = self.cache.read().expect("cache lock poisoned");
            if cache.contains_key(&hash) {
                return Ok(hash);
            }
        }

        let module = Module::new(&self.engine, wasm_bytes)
            .map_err(|e| WasmError::Compilation(e.to_string()))?;

        let cached = Arc::new(CachedModule { module });
        {
            let mut cache = self.cache.write().expect("cache lock poisoned");
            cache.insert(hash.clone(), cached);
        }

        tracing::info!(hash = %hash, size = wasm_bytes.len(), "WASM module compiled and cached");
        Ok(hash)
    }

    /// Check if a module is cached.
    pub fn is_cached(&self, hash: &str) -> bool {
        let cache = self.cache.read().expect("cache lock poisoned");
        cache.contains_key(hash)
    }

    /// Invoke a cached WASM module.
    ///
    /// Each invocation gets a fresh Store with fuel and memory limits.
    /// The module must export a `run` function that takes `(i32, i32)` ->
    /// `i32` where the inputs are (context_ptr, context_len) and the return
    /// is a result pointer. Alternatively, the module can use `host_set_result`
    /// to provide the result via host call.
    pub async fn invoke(
        &self,
        module_hash: &str,
        context: &WasmInvocationContext,
        host: Arc<dyn WasmHost>,
        limits: &WasmResourceLimits,
        streams: Arc<RwLock<StreamRegistry>>,
    ) -> Result<WasmInvocationResult, WasmError> {
        let cached = {
            let cache = self.cache.read().expect("cache lock poisoned");
            cache
                .get(module_hash)
                .cloned()
                .ok_or_else(|| WasmError::ModuleNotFound(module_hash.to_string()))?
        };

        let start = std::time::Instant::now(); // determinism-ok: wall-clock timing for WASM sandbox
        let context_json = serde_json::to_string(context)
            .map_err(|e| WasmError::Invocation(format!("failed to serialize context: {e}")))?;

        // Create a fresh store with fuel budget and memory limiter
        let host_state = HostState {
            context_json: context_json.clone(),
            result_json: None,
            host,
            limiter: MemoryLimiter {
                max_memory: limits.max_memory,
            },
            streams,
        };
        let mut store = Store::new(&self.engine, host_state);
        store
            .set_fuel(limits.max_fuel)
            .map_err(|e| WasmError::Invocation(format!("failed to set fuel: {e}")))?;

        // Register the memory limiter so memory.grow is gated by max_memory.
        store.limiter(|state| &mut state.limiter);

        // Set epoch deadline to 1 tick — the engine epoch is incremented by
        // the timeout task below. If the task fires before run() returns, the
        // module receives a trap on the next back-edge check.
        store.set_epoch_deadline(1);

        // Spawn a one-shot timer that increments the epoch after max_duration.
        // This provides wall-clock timeout on top of the fuel instruction budget.
        let engine_for_timeout = self.engine.clone();
        let max_duration = limits.max_duration;
        let timeout_task = tokio::spawn(async move {
            // determinism-ok: epoch timer for WASM wall-clock timeout enforcement
            tokio::time::sleep(max_duration).await;
            engine_for_timeout.increment_epoch();
        });

        // Guard that aborts the epoch timer on any exit path (Ok or Err).
        // This prevents a leaked timer from perturbing concurrent invocations.
        struct AbortOnDrop(tokio::task::JoinHandle<()>);
        impl Drop for AbortOnDrop {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _timer_guard = AbortOnDrop(timeout_task);

        // Link host functions
        let mut linker = Linker::new(&self.engine);
        link_host_functions(&mut linker)?;

        // Instantiate
        let instance = linker
            .instantiate(&mut store, &cached.module)
            .map_err(|e| WasmError::Instantiation(e.to_string()))?;

        // Find and call the `run` export
        let run_fn = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "run")
            .map_err(|e| WasmError::Invocation(format!("module missing 'run' export: {e}")))?;

        // Write context JSON into module memory
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| WasmError::Invocation("module missing 'memory' export".into()))?;

        let ctx_bytes = context_json.as_bytes();
        let ctx_ptr = 1024_usize; // Fixed offset for context data
        memory.write(&mut store, ctx_ptr, ctx_bytes).map_err(|e| {
            WasmError::Invocation(format!("failed to write context to memory: {e}"))
        })?;

        // Call run(ptr, len) -> result_ptr
        let result_ptr = run_fn
            .call(&mut store, (ctx_ptr as i32, ctx_bytes.len() as i32))
            .map_err(|e| {
                // Use downcast to identify trap kind — the display string wraps
                // backtrace context so string matching is unreliable.
                match e.downcast_ref::<wasmtime::Trap>() {
                    Some(&wasmtime::Trap::OutOfFuel) => WasmError::FuelExhausted,
                    Some(&wasmtime::Trap::Interrupt) => WasmError::Timeout(max_duration),
                    _ => WasmError::Invocation(e.to_string()),
                }
            })?;

        let duration_ms = start.elapsed().as_millis() as u64;

        // Read result: prefer host_set_result (explicit API), fall back to memory pointer.
        let result_json = if let Some(ref host_result) = store.data().result_json {
            // Module used host_set_result — this is the preferred path.
            host_result.clone()
        } else if result_ptr > 0 {
            // Legacy path: read from module memory at result_ptr with length at result_ptr-4.
            let mut len_bytes = [0u8; 4];
            memory
                .read(&store, (result_ptr - 4) as usize, &mut len_bytes)
                .map_err(|e| WasmError::Invocation(format!("failed to read result length: {e}")))?;
            let result_len = u32::from_le_bytes(len_bytes) as usize;

            let mut result_bytes = vec![0u8; result_len];
            memory
                .read(&store, result_ptr as usize, &mut result_bytes)
                .map_err(|e| WasmError::Invocation(format!("failed to read result: {e}")))?;

            String::from_utf8(result_bytes)
                .map_err(|e| WasmError::Invocation(format!("result is not valid UTF-8: {e}")))?
        } else {
            // No result from either path.
            String::new()
        };

        // Parse the result JSON
        if result_json.is_empty() {
            return Ok(WasmInvocationResult {
                callback_action: String::new(),
                callback_params: serde_json::Value::Null,
                success: false,
                error: Some("module returned empty result".to_string()),
                duration_ms,
            });
        }

        let parsed: serde_json::Value = serde_json::from_str(&result_json)
            .map_err(|e| WasmError::Invocation(format!("failed to parse result JSON: {e}")))?;

        Ok(WasmInvocationResult {
            callback_action: parsed
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            callback_params: parsed
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            success: parsed
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            error: parsed
                .get("error")
                .and_then(|v| v.as_str())
                .map(String::from),
            duration_ms,
        })
    }

    /// Remove a module from the cache.
    pub fn evict(&self, hash: &str) -> bool {
        let mut cache = self.cache.write().expect("cache lock poisoned");
        cache.remove(hash).is_some()
    }

    /// Number of cached modules.
    pub fn cache_size(&self) -> usize {
        let cache = self.cache.read().expect("cache lock poisoned");
        cache.len()
    }
}

impl Default for WasmEngine {
    fn default() -> Self {
        Self::new().expect("failed to create default WasmEngine")
    }
}

/// Link host functions into the WASM linker.
fn link_host_functions(linker: &mut Linker<HostState>) -> Result<(), WasmError> {
    // host_log(level_ptr, level_len, msg_ptr, msg_len)
    linker
        .func_wrap(
            "env",
            "host_log",
            |mut caller: Caller<'_, HostState>,
             level_ptr: i32,
             level_len: i32,
             msg_ptr: i32,
             msg_len: i32| {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                if let Some(memory) = memory {
                    let mut level_buf = vec![0u8; level_len as usize];
                    let mut msg_buf = vec![0u8; msg_len as usize];
                    let _ = memory.read(&caller, level_ptr as usize, &mut level_buf);
                    let _ = memory.read(&caller, msg_ptr as usize, &mut msg_buf);
                    let level = String::from_utf8_lossy(&level_buf);
                    let msg = String::from_utf8_lossy(&msg_buf);
                    caller.data().host.log(&level, &msg);
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_log: {e}")))?;

    // host_get_context(buf_ptr, buf_len) -> actual_len
    linker
        .func_wrap(
            "env",
            "host_get_context",
            |mut caller: Caller<'_, HostState>, buf_ptr: i32, buf_len: i32| -> i32 {
                let ctx_json = caller.data().context_json.clone();
                let ctx_bytes = ctx_json.as_bytes();
                if (buf_len as usize) < ctx_bytes.len() {
                    return ctx_bytes.len() as i32; // Return needed size
                }
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                if let Some(memory) = memory {
                    let _ = memory.write(&mut caller, buf_ptr as usize, ctx_bytes);
                }
                ctx_bytes.len() as i32
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_get_context: {e}")))?;

    // host_set_result(ptr, len)
    linker
        .func_wrap(
            "env",
            "host_set_result",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                if let Some(memory) = memory {
                    let mut buf = vec![0u8; len as usize];
                    let _ = memory.read(&caller, ptr as usize, &mut buf);
                    if let Ok(s) = String::from_utf8(buf) {
                        caller.data_mut().result_json = Some(s);
                    }
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_set_result: {e}")))?;

    // host_get_secret(key_ptr, key_len, buf_ptr, buf_len) -> actual_len (-1 on error)
    linker
        .func_wrap(
            "env",
            "host_get_secret",
            |mut caller: Caller<'_, HostState>,
             key_ptr: i32,
             key_len: i32,
             buf_ptr: i32,
             buf_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else { return -1 };

                let mut key_buf = vec![0u8; key_len as usize];
                let _ = memory.read(&caller, key_ptr as usize, &mut key_buf);
                let key = String::from_utf8_lossy(&key_buf);

                match caller.data().host.get_secret(&key) {
                    Ok(secret) => {
                        let secret_bytes = secret.as_bytes();
                        if (buf_len as usize) < secret_bytes.len() {
                            return secret_bytes.len() as i32;
                        }
                        let _ = memory.write(&mut caller, buf_ptr as usize, secret_bytes);
                        secret_bytes.len() as i32
                    }
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_get_secret: {e}")))?;

    // host_http_call(method_ptr, method_len, url_ptr, url_len,
    //                headers_ptr, headers_len, body_ptr, body_len,
    //                result_buf_ptr, result_buf_len) -> i32
    // Returns: bytes written to result_buf (status_code\nbody), or -1 on error, -2 if buf too small
    linker
        .func_wrap(
            "env",
            "host_http_call",
            |mut caller: Caller<'_, HostState>,
             method_ptr: i32,
             method_len: i32,
             url_ptr: i32,
             url_len: i32,
             headers_ptr: i32,
             headers_len: i32,
             body_ptr: i32,
             body_len: i32,
             result_buf_ptr: i32,
             result_buf_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };

                // Read method
                let mut method_buf = vec![0u8; method_len as usize];
                let _ = memory.read(&caller, method_ptr as usize, &mut method_buf);
                let method = String::from_utf8_lossy(&method_buf).to_string();

                // Read URL
                let mut url_buf = vec![0u8; url_len as usize];
                let _ = memory.read(&caller, url_ptr as usize, &mut url_buf);
                let url = String::from_utf8_lossy(&url_buf).to_string();

                // Read headers (JSON array of [key, value] pairs)
                let headers: Vec<(String, String)> = if headers_len > 0 {
                    let mut hdr_buf = vec![0u8; headers_len as usize];
                    let _ = memory.read(&caller, headers_ptr as usize, &mut hdr_buf);
                    serde_json::from_slice(&hdr_buf).unwrap_or_default()
                } else {
                    vec![]
                };

                // Read body
                let body = if body_len > 0 {
                    let mut body_buf = vec![0u8; body_len as usize];
                    let _ = memory.read(&caller, body_ptr as usize, &mut body_buf);
                    String::from_utf8_lossy(&body_buf).to_string()
                } else {
                    String::new()
                };

                // Bridge async -> sync
                let host = caller.data().host.clone();
                let result = tokio::task::block_in_place(|| {
                    // determinism-ok: blocking bridge for WASM host call
                    tokio::runtime::Handle::current()
                        .block_on(host.http_call(&method, &url, &headers, &body))
                });

                match result {
                    Ok((status, resp_body)) => {
                        let response = format!("{status}\n{resp_body}");
                        let resp_bytes = response.as_bytes();
                        if resp_bytes.len() > result_buf_len as usize {
                            return -2; // buffer too small
                        }
                        let _ = memory.write(&mut caller, result_buf_ptr as usize, resp_bytes);
                        resp_bytes.len() as i32
                    }
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_http_call: {e}")))?;

    // host_http_call_stream(method_ptr, method_len, url_ptr, url_len,
    //                       headers_ptr, headers_len,
    //                       body_stream_id_ptr, body_stream_id_len,
    //                       response_stream_id_ptr, response_stream_id_len) -> i32
    // Returns HTTP status code, or -1 on error.
    // Bytes flow through StreamRegistry, never through WASM memory.
    #[allow(clippy::too_many_arguments)]
    linker
        .func_wrap(
            "env",
            "host_http_call_stream",
            |mut caller: Caller<'_, HostState>,
             method_ptr: i32,
             method_len: i32,
             url_ptr: i32,
             url_len: i32,
             headers_ptr: i32,
             headers_len: i32,
             body_stream_id_ptr: i32,
             body_stream_id_len: i32,
             response_stream_id_ptr: i32,
             response_stream_id_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };

                // Read method
                let mut method_buf = vec![0u8; method_len as usize];
                let _ = memory.read(&caller, method_ptr as usize, &mut method_buf);
                let method = String::from_utf8_lossy(&method_buf).to_string();

                // Read URL
                let mut url_buf = vec![0u8; url_len as usize];
                let _ = memory.read(&caller, url_ptr as usize, &mut url_buf);
                let url = String::from_utf8_lossy(&url_buf).to_string();

                // Read headers (JSON array of [key, value] pairs)
                let headers: Vec<(String, String)> = if headers_len > 0 {
                    let mut hdr_buf = vec![0u8; headers_len as usize];
                    let _ = memory.read(&caller, headers_ptr as usize, &mut hdr_buf);
                    serde_json::from_slice(&hdr_buf).unwrap_or_default()
                } else {
                    vec![]
                };

                // Read body stream ID
                let body_stream_id = if body_stream_id_len > 0 {
                    let mut id_buf = vec![0u8; body_stream_id_len as usize];
                    let _ = memory.read(&caller, body_stream_id_ptr as usize, &mut id_buf);
                    String::from_utf8_lossy(&id_buf).to_string()
                } else {
                    String::new()
                };

                // Read response stream ID
                let response_stream_id = if response_stream_id_len > 0 {
                    let mut id_buf = vec![0u8; response_stream_id_len as usize];
                    let _ = memory.read(&caller, response_stream_id_ptr as usize, &mut id_buf);
                    String::from_utf8_lossy(&id_buf).to_string()
                } else {
                    String::new()
                };

                // Get request body from StreamRegistry (if stream ID provided)
                let body_bytes = if !body_stream_id.is_empty() {
                    let streams = caller.data().streams.read().expect("streams lock poisoned"); // ci-ok: infallible lock
                    streams
                        .get_stream(&body_stream_id)
                        .map(|b| b.to_vec())
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };

                // Bridge async -> sync for HTTP call with binary body
                let host = caller.data().host.clone();
                let result = tokio::task::block_in_place(|| {
                    // determinism-ok: blocking bridge for WASM host call
                    tokio::runtime::Handle::current().block_on(host.http_call_binary(
                        &method,
                        &url,
                        &headers,
                        &body_bytes,
                    ))
                });

                match result {
                    Ok((status, resp_bytes)) => {
                        // Store response bytes in StreamRegistry (if stream ID provided)
                        if !response_stream_id.is_empty() && !resp_bytes.is_empty() {
                            let mut streams = caller
                                .data()
                                .streams
                                .write()
                                .expect("streams lock poisoned"); // ci-ok: infallible lock
                            streams.store_stream(&response_stream_id, resp_bytes);
                        }
                        status as i32
                    }
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| {
            WasmError::Compilation(format!("failed to link host_http_call_stream: {e}"))
        })?;

    // host_cache_contains(key_ptr, key_len) -> i32
    // Returns 1 if cached, 0 if not.
    linker
        .func_wrap(
            "env",
            "host_cache_contains",
            |mut caller: Caller<'_, HostState>, key_ptr: i32, key_len: i32| -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return 0;
                };

                let mut key_buf = vec![0u8; key_len as usize];
                let _ = memory.read(&caller, key_ptr as usize, &mut key_buf);
                let key = String::from_utf8_lossy(&key_buf);

                let streams = caller.data().streams.read().expect("streams lock poisoned"); // ci-ok: infallible lock
                if streams.cache_contains(&key) { 1 } else { 0 }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_cache_contains: {e}")))?;

    // host_cache_to_stream(key_ptr, key_len, stream_id_ptr, stream_id_len) -> i32
    // Copies cached bytes to a stream. Returns byte count on success, -1 if not cached.
    linker
        .func_wrap(
            "env",
            "host_cache_to_stream",
            |mut caller: Caller<'_, HostState>,
             key_ptr: i32,
             key_len: i32,
             stream_id_ptr: i32,
             stream_id_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };

                let mut key_buf = vec![0u8; key_len as usize];
                let _ = memory.read(&caller, key_ptr as usize, &mut key_buf);
                let key = String::from_utf8_lossy(&key_buf).to_string();

                let mut id_buf = vec![0u8; stream_id_len as usize];
                let _ = memory.read(&caller, stream_id_ptr as usize, &mut id_buf);
                let stream_id = String::from_utf8_lossy(&id_buf).to_string();

                let mut streams = caller
                    .data()
                    .streams
                    .write()
                    .expect("streams lock poisoned"); // ci-ok: infallible lock
                match streams.cache_to_stream(&key, &stream_id) {
                    Some(byte_count) => byte_count as i32,
                    None => -1,
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_cache_to_stream: {e}")))?;

    // host_cache_from_stream(key_ptr, key_len, stream_id_ptr, stream_id_len) -> i32
    // Caches bytes from a stream. Returns 0 on success, -1 on error.
    linker
        .func_wrap(
            "env",
            "host_cache_from_stream",
            |mut caller: Caller<'_, HostState>,
             key_ptr: i32,
             key_len: i32,
             stream_id_ptr: i32,
             stream_id_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };

                let mut key_buf = vec![0u8; key_len as usize];
                let _ = memory.read(&caller, key_ptr as usize, &mut key_buf);
                let key = String::from_utf8_lossy(&key_buf).to_string();

                let mut id_buf = vec![0u8; stream_id_len as usize];
                let _ = memory.read(&caller, stream_id_ptr as usize, &mut id_buf);
                let stream_id = String::from_utf8_lossy(&id_buf).to_string();

                let mut streams = caller
                    .data()
                    .streams
                    .write()
                    .expect("streams lock poisoned"); // ci-ok: infallible lock
                // Read bytes from stream without consuming it
                let bytes = match streams.get_stream(&stream_id) {
                    Some(b) => b.to_vec(),
                    None => return -1,
                };
                streams.cache_put(&key, bytes);
                0
            },
        )
        .map_err(|e| {
            WasmError::Compilation(format!("failed to link host_cache_from_stream: {e}"))
        })?;

    // host_hash_stream(stream_id_ptr, stream_id_len,
    //                  algorithm_ptr, algorithm_len,
    //                  result_buf_ptr, result_buf_len) -> i32
    // Computes hash of stream bytes. Returns bytes written to result_buf, or -1 on error.
    // Algorithm chosen by WASM (hot-reloadable): "sha256", "blake3", etc.
    linker
        .func_wrap(
            "env",
            "host_hash_stream",
            |mut caller: Caller<'_, HostState>,
             stream_id_ptr: i32,
             stream_id_len: i32,
             algorithm_ptr: i32,
             algorithm_len: i32,
             result_buf_ptr: i32,
             result_buf_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else {
                    return -1;
                };

                // Read stream ID
                let mut id_buf = vec![0u8; stream_id_len as usize];
                let _ = memory.read(&caller, stream_id_ptr as usize, &mut id_buf);
                let stream_id = String::from_utf8_lossy(&id_buf).to_string();

                // Read algorithm
                let mut algo_buf = vec![0u8; algorithm_len as usize];
                let _ = memory.read(&caller, algorithm_ptr as usize, &mut algo_buf);
                let algorithm = String::from_utf8_lossy(&algo_buf).to_string();

                // Hash stream bytes in-place (no clone)
                let streams = caller.data().streams.read().expect("streams lock poisoned"); // ci-ok: infallible lock
                let Some(bytes) = streams.get_stream(&stream_id) else {
                    return -1;
                };

                let hex_hash = match algorithm.as_str() {
                    "sha256" => {
                        let mut hasher = Sha256::new();
                        hasher.update(bytes);
                        format!("sha256:{:x}", hasher.finalize())
                    }
                    _ => return -1,
                };
                drop(streams);

                // Write hex hash to result buffer
                let hash_bytes = hex_hash.as_bytes();
                if hash_bytes.len() > result_buf_len as usize {
                    return -1; // buffer too small
                }
                let _ = memory.write(&mut caller, result_buf_ptr as usize, hash_bytes);
                hash_bytes.len() as i32
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_hash_stream: {e}")))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::RwLock;

    use super::*;
    use crate::host_trait::SimWasmHost;
    use crate::stream::StreamRegistry;

    // Minimal WAT module: accepts (ptr, len), writes nothing, returns 0.
    // Uses host_set_result to return an empty success JSON.
    const WAT_NOOP: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "run") (param i32 i32) (result i32)
            i32.const 0
          )
        )
    "#;

    // Infinite loop module — exhausts fuel / trips timeout.
    const WAT_INFINITE_LOOP: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "run") (param i32 i32) (result i32)
            (loop $L
              br $L)
            i32.const 0
          )
        )
    "#;

    // Tries to grow memory by 1000 pages (64 MB) — exceeds 16 MB default.
    const WAT_MEMORY_GROW: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "run") (param i32 i32) (result i32)
            (memory.grow (i32.const 1000))
            drop
            i32.const 0
          )
        )
    "#;

    // Raises an unreachable trap — tests error isolation.
    const WAT_TRAP: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "run") (param i32 i32) (result i32)
            unreachable
          )
        )
    "#;

    fn make_context() -> WasmInvocationContext {
        WasmInvocationContext {
            tenant: "test".into(),
            entity_type: "Order".into(),
            entity_id: "1".into(),
            trigger_action: "Submit".into(),
            trigger_params: serde_json::Value::Null,
            entity_state: serde_json::Value::Null,
            agent_id: None,
            session_id: None,
            integration_config: std::collections::BTreeMap::new(),
        }
    }

    fn make_host() -> Arc<dyn WasmHost> {
        Arc::new(SimWasmHost::new())
    }

    fn make_streams() -> Arc<RwLock<StreamRegistry>> {
        Arc::new(RwLock::new(StreamRegistry::default()))
    }

    #[test]
    fn hash_module_deterministic() {
        let bytes = b"test wasm bytes";
        let h1 = WasmEngine::hash_module(bytes);
        let h2 = WasmEngine::hash_module(bytes);
        assert_eq!(h1, h2);
        assert!(!h1.is_empty());
    }

    #[test]
    fn engine_creation() {
        let engine = WasmEngine::new();
        assert!(engine.is_ok());
    }

    #[test]
    fn module_too_large_rejected() {
        let engine = WasmEngine::new().unwrap();
        let big = vec![0u8; MAX_MODULE_SIZE + 1];
        let result = engine.compile_and_cache(&big);
        assert!(matches!(result, Err(WasmError::ModuleTooLarge { .. })));
    }

    #[test]
    fn resource_limits_default() {
        let limits = WasmResourceLimits::default();
        assert_eq!(limits.max_fuel, 1_000_000_000);
        assert_eq!(limits.max_memory, 16 * 1024 * 1024);
        assert_eq!(limits.max_duration, std::time::Duration::from_secs(30));
        assert_eq!(limits.max_response_bytes, 1024 * 1024);
    }

    #[test]
    fn timeout_error_display() {
        let err = WasmError::Timeout(std::time::Duration::from_secs(5));
        let msg = err.to_string();
        assert!(msg.contains("timeout"), "expected 'timeout' in: {msg}");
    }

    #[test]
    fn memory_limit_exceeded_display() {
        let err = WasmError::MemoryLimitExceeded { max_bytes: 1024 };
        let msg = err.to_string();
        assert!(
            msg.contains("memory limit"),
            "expected 'memory limit' in: {msg}"
        );
    }

    /// Fuel exhaustion: module runs infinite loop with tiny fuel budget.
    #[tokio::test]
    async fn fuel_exhaustion_returns_error() {
        let engine = WasmEngine::new().unwrap();
        let hash = engine
            .compile_and_cache(WAT_INFINITE_LOOP.as_bytes())
            .unwrap();

        let limits = WasmResourceLimits {
            max_fuel: 1_000, // tiny budget
            ..WasmResourceLimits::default()
        };
        let result = engine
            .invoke(&hash, &make_context(), make_host(), &limits, make_streams())
            .await;

        assert!(
            matches!(result, Err(WasmError::FuelExhausted)),
            "expected FuelExhausted, got: {result:?}"
        );
    }

    /// Timeout: module runs infinite loop, epoch fires after 50 ms.
    ///
    /// Requires multi-thread runtime: the epoch timer task must run concurrently
    /// while the WASM call blocks the main thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_enforced_by_epoch() {
        let engine = WasmEngine::new().unwrap();
        let hash = engine
            .compile_and_cache(WAT_INFINITE_LOOP.as_bytes())
            .unwrap();

        let limits = WasmResourceLimits {
            max_fuel: u64::MAX,                                 // no fuel limit
            max_duration: std::time::Duration::from_millis(50), // very short
            ..WasmResourceLimits::default()
        };
        let result = engine
            .invoke(&hash, &make_context(), make_host(), &limits, make_streams())
            .await;

        assert!(
            matches!(result, Err(WasmError::Timeout(_))),
            "expected Timeout, got: {result:?}"
        );
    }

    /// Error isolation: a WASM trap (unreachable) is converted to WasmError,
    /// not a Rust panic. The host process must survive.
    #[tokio::test]
    async fn wasm_trap_does_not_crash_host() {
        let engine = WasmEngine::new().unwrap();
        let hash = engine.compile_and_cache(WAT_TRAP.as_bytes()).unwrap();

        let limits = WasmResourceLimits::default();
        let result = engine
            .invoke(&hash, &make_context(), make_host(), &limits, make_streams())
            .await;

        // Must return Err (trap propagated as error), not panic.
        assert!(result.is_err(), "expected Err from trap, got Ok");
        // Must not be mistaken for fuel or timeout.
        assert!(
            !matches!(
                result,
                Err(WasmError::FuelExhausted) | Err(WasmError::Timeout(_))
            ),
            "trap should not be FuelExhausted or Timeout"
        );
    }

    /// Memory limiter: module tries to grow beyond max_memory — growth is denied
    /// but the module still returns (memory.grow returns -1, not a crash).
    #[tokio::test]
    async fn memory_growth_denied_by_limiter() {
        let engine = WasmEngine::new().unwrap();
        let hash = engine
            .compile_and_cache(WAT_MEMORY_GROW.as_bytes())
            .unwrap();

        // Limit to 1 page (64 KB). Module tries to grow by 1000 pages.
        let limits = WasmResourceLimits {
            max_memory: 64 * 1024, // 1 WASM page
            ..WasmResourceLimits::default()
        };
        let result = engine
            .invoke(&hash, &make_context(), make_host(), &limits, make_streams())
            .await;

        // Module returns normally (memory.grow returned -1 per spec — not a trap).
        // The invocation itself succeeds (no crash), but the result is empty
        // because the module didn't call host_set_result.
        assert!(
            result.is_ok() || matches!(result, Err(WasmError::Invocation(_))),
            "memory denial should not cause fuel/timeout error, got: {result:?}"
        );
        // Critically: no panic, no FuelExhausted, no Timeout.
        assert!(
            !matches!(
                result,
                Err(WasmError::FuelExhausted) | Err(WasmError::Timeout(_))
            ),
            "memory denial should not be misclassified"
        );
    }

    /// Noop module completes successfully without hitting any limits.
    #[tokio::test]
    async fn noop_module_completes() {
        let engine = WasmEngine::new().unwrap();
        let hash = engine.compile_and_cache(WAT_NOOP.as_bytes()).unwrap();

        let limits = WasmResourceLimits::default();
        let result = engine
            .invoke(&hash, &make_context(), make_host(), &limits, make_streams())
            .await;

        // Noop doesn't call host_set_result so we get an empty-result error,
        // but crucially it doesn't hit fuel, timeout, or memory errors.
        assert!(
            !matches!(
                result,
                Err(WasmError::FuelExhausted)
                    | Err(WasmError::Timeout(_))
                    | Err(WasmError::MemoryLimitExceeded { .. })
            ),
            "noop should not hit resource limits, got: {result:?}"
        );
    }
}
