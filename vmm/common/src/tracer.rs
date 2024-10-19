/*
Copyright 2024 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::anyhow;
use lazy_static::lazy_static;
use log::{debug, info, warn};
use nix::libc::{SIGINT, SIGTERM, SIGUSR1};
use opentelemetry::{
    global,
    sdk::{
        trace::{self, Tracer},
        Resource,
    },
    trace::noop::NoopTracer,
};
use signal_hook::iterator::Signals;
use tracing_subscriber::{
    layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer, Registry,
};

lazy_static! {
    static ref ENABLED: AtomicBool = AtomicBool::new(false);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

// 设置 ENABLED 的值
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn setup_tracing(
    log_level: &str,
    enable_tracing: bool,
    otlp_service_name: &str,
) -> anyhow::Result<()> {
    let env_filter = init_logger_filter(log_level)
        .map_err(|e| anyhow!("failed to init logger filter: {}", e))?;

    let mut layers = vec![tracing_subscriber::fmt::layer().boxed()];

    if enable_tracing {
        let tracer = init_otlp_tracer(otlp_service_name)
            .map_err(|e| anyhow!("failed to init otlp tracer: {}", e))?;
        layers.push(tracing_opentelemetry::layer().with_tracer(tracer).boxed());
    } else {
        layers.push(
            tracing_opentelemetry::layer()
                .with_tracer(NoopTracer::new())
                .boxed(),
        );
    }

    Registry::default().with(env_filter).with(layers).init();
    Ok(())
}

fn init_logger_filter(log_level: &str) -> anyhow::Result<EnvFilter> {
    let filter = EnvFilter::from_default_env()
        .add_directive(format!("containerd_sandbox={}", log_level).parse()?)
        .add_directive(format!("vmm_sandboxer={}", log_level).parse()?);
    Ok(filter)
}

pub fn init_otlp_tracer(otlp_service_name: &str) -> anyhow::Result<Tracer> {
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(opentelemetry_otlp::new_exporter().tonic())
        .with_trace_config(trace::config().with_resource(Resource::new(vec![
            opentelemetry::KeyValue::new("service.name", otlp_service_name.to_string()),
        ])))
        .install_batch(opentelemetry::runtime::Tokio)?;
    Ok(tracer)
}

pub fn shutdown_tracing() {
    set_enabled(false);
    global::shutdown_tracer_provider();
}

pub async fn handle_signals(log_level: &str, otlp_service_name: &str) {
    let mut signals = Signals::new([SIGTERM, SIGINT, SIGUSR1]).expect("new signal failed");
    for sig in signals.forever() {
        match sig {
            SIGUSR1 => {
                set_enabled(!enabled());
                let _ = setup_tracing(log_level, enabled(), otlp_service_name);
            }
            SIGINT | SIGTERM => {
                // 处理退出信号，执行清理操作并退出
                info!("Received exit signal, stopping tracing and exiting...");
                shutdown_tracing();
                std::process::exit(0); // 优雅退出
            }
            _ => {
                if let Ok(sig) = nix::sys::signal::Signal::try_from(sig) {
                    debug!("received {}", sig);
                } else {
                    warn!("received invalid signal {}", sig);
                }
            }
        }
    }
}
