//! WASM plugin host: load and run plugins with wasmtime, fuel and memory limits, host imports.

use crate::abi::{export_names, import_names};
use crate::registry::PluginManifest;
use anyhow::{Context, anyhow};
use wasmtime::*;

/// Resource limits and capability toggles applied to every plugin call.
#[derive(Clone)]
pub struct SandboxConfig {
    pub allow_filesystem: bool,
    pub allow_network: bool,
    pub max_memory_bytes: usize,
    /// Max execution time: fuel units (roughly 1 per instruction). Approximate secs = fuel / 1_000_000.
    pub max_fuel: u64,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            allow_filesystem: false,
            allow_network: false,
            max_memory_bytes: 64 * 1024 * 1024, // 64MB
            max_fuel: 10_000_000,               // ~10M instructions
        }
    }
}

impl SandboxConfig {
    /// Approximate fuel for N seconds of execution (very rough).
    pub fn fuel_for_secs(secs: u64) -> u64 {
        secs.saturating_mul(1_000_000)
    }
}

/// Per-plugin loaded module and type info. Call state is in Store.
#[derive(Clone)]
pub struct LoadedPlugin {
    module: Module,
}

/// Host state for host imports (log sink, config, limiter).
pub struct HostState {
    pub sandbox_config: SandboxConfig,
    pub log_sink: Vec<String>,
    pub limits: wasmtime::StoreLimits,
    /// Length of input args written at offset 0, used to compute safe result offset.
    pub input_len: usize,
}

/// Loads and runs WASM plugins under wasmtime with the configured sandbox
/// limits; holds the wasmtime `Engine` and the set of currently loaded
/// modules.
pub struct WasmHost {
    engine: Engine,
    sandbox_config: SandboxConfig,
    /// Cache of loaded modules by path or name.
    plugins: std::sync::RwLock<std::collections::HashMap<String, LoadedPlugin>>,
}

impl WasmHost {
    pub fn new(sandbox_config: SandboxConfig) -> anyhow::Result<Self> {
        let mut config = Config::new();
        // Fuel metering must be enabled up front on the engine Config — it
        // can't be turned on per-Store later, and call_plugin() relies on it
        // to cap execution length.
        config.consume_fuel(true);
        let engine = Engine::new(&config).context("wasmtime Engine::new")?;
        Ok(Self {
            engine,
            sandbox_config: sandbox_config.clone(),
            plugins: std::sync::RwLock::new(std::collections::HashMap::new()),
        })
    }

    /// Load a WASM module from path and register it under a name.
    pub fn load_plugin(&self, name: impl Into<String>, wasm_path: &str) -> anyhow::Result<()> {
        let name = name.into();
        let bytes =
            std::fs::read(wasm_path).with_context(|| format!("read wasm: {}", wasm_path))?;
        self.load_plugin_bytes(&name, &bytes)
    }

    /// Load a WASM module from bytes.
    pub fn load_plugin_bytes(&self, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let module = Module::new(&self.engine, bytes).context("wasmtime Module::new")?;
        let loaded = LoadedPlugin { module };
        self.plugins
            .write()
            .unwrap()
            .insert(name.to_string(), loaded);
        Ok(())
    }

    /// Call an exported function on a loaded plugin. Pass args as JSON bytes; returns result bytes.
    /// Convention: plugin_execute(args_ptr, args_len) returns ptr to length-prefixed result (4 bytes LE length + payload).
    pub fn call_plugin(&self, plugin_name: &str, args: &[u8]) -> anyhow::Result<Vec<u8>> {
        let plugin = self
            .plugins
            .read()
            .unwrap()
            .get(plugin_name)
            .cloned()
            .ok_or_else(|| anyhow!("plugin not loaded: {}", plugin_name))?;

        let limits = wasmtime::StoreLimitsBuilder::new()
            .memory_size(self.sandbox_config.max_memory_bytes)
            .trap_on_grow_failure(true)
            .build();
        let state = HostState {
            sandbox_config: self.sandbox_config.clone(),
            log_sink: Vec::new(),
            limits,
            input_len: 0,
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s: &mut HostState| &mut s.limits);
        store.set_fuel(self.sandbox_config.max_fuel)?;

        let mut linker = Linker::new(&self.engine);

        // host_log(level: i32, msg_ptr: i32, msg_len: i32) -> ()
        linker.func_wrap(
            "env",
            import_names::LOG,
            |mut caller: Caller<'_, HostState>, level: i32, msg_ptr: i32, msg_len: i32| {
                let memory = caller
                    .get_export("memory")
                    .and_then(|e| e.into_memory())
                    .ok_or_else(|| anyhow::anyhow!("no memory"))?;
                let mut buf = vec![0u8; msg_len as usize];
                memory.read(&caller, msg_ptr as usize, &mut buf)?;
                let msg = String::from_utf8_lossy(&buf);
                let level_str = match level {
                    0 => "trace",
                    1 => "debug",
                    2 => "info",
                    3 => "warn",
                    _ => "error",
                };
                caller
                    .data_mut()
                    .log_sink
                    .push(format!("[{}] {}", level_str, msg));
                tracing::info!(level = %level_str, msg = %msg, "plugin_log");
                Ok(())
            },
        )?;

        // host_read_file(path_ptr, path_len) -> result_ptr (0 = error). Result = length-prefixed bytes.
        if self.sandbox_config.allow_filesystem {
            linker.func_wrap(
                "env",
                import_names::READ_FILE,
                |mut caller: Caller<'_, HostState>, path_ptr: i32, path_len: i32| -> Result<i32> {
                    let memory = caller
                        .get_export("memory")
                        .and_then(|e| e.into_memory())
                        .ok_or_else(|| anyhow::anyhow!("no memory"))?;
                    let mut buf = vec![0u8; path_len as usize];
                    memory.read(&caller, path_ptr as usize, &mut buf)?;
                    let path = String::from_utf8_lossy(&buf);
                    let content = std::fs::read(path.as_ref())
                        .map_err(|_| anyhow::anyhow!("read file failed"))?;
                    let max_size = 1024 * 1024; // 1MB
                    let content = if content.len() > max_size {
                        content[..max_size].to_vec()
                    } else {
                        content
                    };
                    let input_len = caller.data().input_len;
                    let result_offset = (input_len + 3) & !3; // 4-byte aligned after input
                    let total = 4 + content.len();
                    if memory.data_size(&caller) < result_offset + total {
                        return Ok(-1);
                    }
                    let len_bytes = (content.len() as u32).to_le_bytes();
                    memory.write(&mut caller, result_offset, &len_bytes)?;
                    memory.write(&mut caller, result_offset + 4, &content)?;
                    Ok(result_offset as i32)
                },
            )?;
        } else {
            linker.func_wrap(
                "env",
                import_names::READ_FILE,
                |_caller: Caller<'_, HostState>, _path_ptr: i32, _path_len: i32| -> Result<i32> {
                    Ok(-1)
                },
            )?;
        }

        // host_http_request(req_ptr, req_len) -> result_ptr (0 = error). Result = length-prefixed JSON.
        if self.sandbox_config.allow_network {
            linker.func_wrap(
                "env",
                import_names::HTTP_REQUEST,
                |mut caller: Caller<'_, HostState>, req_ptr: i32, req_len: i32| -> Result<i32> {
                    let memory = caller
                        .get_export("memory")
                        .and_then(|e| e.into_memory())
                        .ok_or_else(|| anyhow::anyhow!("no memory"))?;
                    let mut buf = vec![0u8; req_len as usize];
                    memory.read(&caller, req_ptr as usize, &mut buf)?;
                    let req_json: serde_json::Value = serde_json::from_slice(&buf)
                        .map_err(|e| anyhow::anyhow!("invalid request JSON: {}", e))?;

                    let method = req_json["method"].as_str().unwrap_or("GET");
                    let url = req_json["url"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("missing url"))?;
                    let headers = req_json["headers"].as_object();
                    let body = req_json["body"].as_str();

                    let client = reqwest::blocking::Client::new();
                    let mut builder =
                        client.request(method.parse().unwrap_or(reqwest::Method::GET), url);
                    if let Some(hs) = headers {
                        for (k, v) in hs {
                            if let Some(v_str) = v.as_str()
                                && let Ok(hv) = v_str.parse::<reqwest::header::HeaderValue>()
                            {
                                builder = builder.header(k, hv);
                            }
                        }
                    }
                    if let Some(b) = body {
                        builder = builder.body(b.to_string());
                    }

                    let resp = builder
                        .send()
                        .map_err(|e| anyhow::anyhow!("http request failed: {}", e))?;
                    let status = resp.status().as_u16();
                    let resp_headers: serde_json::Map<String, serde_json::Value> = resp
                        .headers()
                        .iter()
                        .filter_map(|(k, v)| {
                            v.to_str().ok().map(|vs| {
                                (
                                    k.as_str().to_string(),
                                    serde_json::Value::String(vs.to_string()),
                                )
                            })
                        })
                        .collect();
                    let resp_body = resp.text().unwrap_or_default();

                    let resp_json = serde_json::json!({
                        "status": status,
                        "headers": resp_headers,
                        "body": resp_body,
                    });
                    let resp_bytes = serde_json::to_vec(&resp_json)
                        .map_err(|e| anyhow::anyhow!("serialize response: {}", e))?;
                    let input_len = caller.data().input_len;
                    let result_offset = (input_len + 3) & !3; // 4-byte aligned after input
                    let total = 4 + resp_bytes.len();
                    if memory.data_size(&caller) < result_offset + total {
                        return Ok(-1);
                    }
                    let len_bytes = (resp_bytes.len() as u32).to_le_bytes();
                    memory.write(&mut caller, result_offset, &len_bytes)?;
                    memory.write(&mut caller, result_offset + 4, &resp_bytes)?;
                    Ok(result_offset as i32)
                },
            )?;
        } else {
            linker.func_wrap(
                "env",
                import_names::HTTP_REQUEST,
                |_caller: Caller<'_, HostState>, _req_ptr: i32, _req_len: i32| -> Result<i32> {
                    Ok(-1)
                },
            )?;
        }

        let instance = linker.instantiate(&mut store, &plugin.module)?;

        // plugin_init() -> i32
        let init = instance
            .get_func(&mut store, export_names::INIT)
            .ok_or_else(|| anyhow!("plugin missing export: {}", export_names::INIT))?;
        let init_typed = init.typed::<(), i32>(&store)?;
        let code = init_typed.call(&mut store, ())?;
        if code != 0 {
            anyhow::bail!("plugin_init returned {}", code);
        }

        // plugin_execute(args_ptr: i32, args_len: i32) -> i32 (ptr to length-prefixed result)
        let execute = instance
            .get_func(&mut store, export_names::EXECUTE)
            .ok_or_else(|| anyhow!("plugin missing export: {}", export_names::EXECUTE))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow!("plugin has no memory"))?;

        // Write args at offset 0
        let args_len = args.len() as i32;
        if memory.data_size(&store) < args.len() {
            anyhow::bail!("plugin memory too small for args");
        }
        memory.write(&mut store, 0, args)?;
        store.data_mut().input_len = args.len();

        let execute_typed = execute.typed::<(i32, i32), i32>(&store)?;
        let result_ptr = execute_typed.call(&mut store, (0i32, args_len))?;

        if result_ptr == 0 {
            return Ok(Vec::new());
        }

        // Read length-prefixed result: [len: u32 LE][data]
        let ptr = result_ptr as usize;
        let mut len_buf = [0u8; 4];
        memory.read(&store, ptr, &mut len_buf)?;
        let result_len = u32::from_le_bytes(len_buf) as usize;
        let max_result = 1024 * 1024; // 1MB cap
        let result_len = result_len.min(max_result);
        let mut out = vec![0u8; result_len];
        memory.read(&store, ptr + 4, &mut out)?;
        Ok(out)
    }

    /// Get manifest for a loaded plugin by calling plugin_info (if exported).
    pub fn plugin_info(&self, plugin_name: &str) -> anyhow::Result<Option<PluginManifest>> {
        let plugin = self
            .plugins
            .read()
            .unwrap()
            .get(plugin_name)
            .cloned()
            .ok_or_else(|| anyhow!("plugin not loaded: {}", plugin_name))?;

        let limits = wasmtime::StoreLimitsBuilder::new()
            .memory_size(self.sandbox_config.max_memory_bytes)
            .trap_on_grow_failure(true)
            .build();
        let state = HostState {
            sandbox_config: self.sandbox_config.clone(),
            log_sink: Vec::new(),
            limits,
            input_len: 0,
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s: &mut HostState| &mut s.limits);
        store.set_fuel(100_000)?;

        let mut linker = Linker::new(&self.engine);
        linker.func_wrap(
            "env",
            import_names::LOG,
            |_caller: Caller<'_, HostState>, _: i32, _: i32, _: i32| Ok(()),
        )?;
        linker.func_wrap(
            "env",
            import_names::READ_FILE,
            |_caller: Caller<'_, HostState>, _: i32, _: i32| Ok(-1i32),
        )?;
        linker.func_wrap(
            "env",
            import_names::HTTP_REQUEST,
            |_caller: Caller<'_, HostState>, _: i32, _: i32| Ok(-1i32),
        )?;

        let instance = linker.instantiate(&mut store, &plugin.module)?;
        let init = instance
            .get_func(&mut store, export_names::INIT)
            .ok_or_else(|| anyhow!("plugin missing init"))?;
        let init_typed = init.typed::<(), i32>(&store)?;
        init_typed.call(&mut store, ())?;

        let info_fn = match instance.get_func(&mut store, export_names::INFO) {
            Some(f) => f,
            None => return Ok(None),
        };
        let info_typed = info_fn.typed::<(), i32>(&store)?;
        let ptr = info_typed.call(&mut store, ())?;
        if ptr == 0 {
            return Ok(None);
        }
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow!("no memory"))?;
        let mut len_buf = [0u8; 4];
        memory.read(&store, ptr as usize, &mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let len = len.min(64 * 1024);
        let mut buf = vec![0u8; len];
        memory.read(&store, ptr as usize + 4, &mut buf)?;
        let manifest: PluginManifest = serde_json::from_slice(&buf).context("plugin_info JSON")?;
        Ok(Some(manifest))
    }

    /// Names of plugins currently loaded in memory.
    pub fn list_loaded(&self) -> Vec<String> {
        self.plugins.read().unwrap().keys().cloned().collect()
    }
}
