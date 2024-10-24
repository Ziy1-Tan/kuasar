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

use nix::libc::{SIGINT, SIGTERM, SIGUSR1};
use signal_hook::iterator::Signals;

use crate::tracer::{enabled, set_enabled, setup_tracing};

pub async fn handle_signals(log_level: &str, otlp_service_name: &str) {
    let mut signals = Signals::new([SIGTERM, SIGINT, SIGUSR1]).expect("new signal failed");

    for sig in signals.forever() {
        if sig == SIGUSR1 {
            set_enabled(!enabled());
            let _ = setup_tracing(log_level, otlp_service_name);
        }
    }
}
