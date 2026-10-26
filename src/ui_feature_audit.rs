//! Nondefault verification hooks that drive real window state in the running
//! application and capture the full UI (panels included). They use the same
//! state fields and event JSON as the egui controls; nothing here runs in an
//! ordinary session.
use anyhow::{ensure, Context, Result};
use ferrite_ui::llm_chat_ui::{Role, Status};
use std::{
    path::Path,
    time::{Duration, Instant},
};

impl super::ChartApp {
    /// One frame of the parts of the event loop these audits depend on.
    fn audit_frame(&mut self) -> Result<()> {
        self.service_mcp_ui();
        self.service_llm_chat_ui();
        let renderer = self.renderer.as_mut().context("Renderer not initialized")?;
        renderer.render()?;
        Ok(())
    }
    fn audit_wait(
        &mut self,
        what: &str,
        limit: Duration,
        mut done: impl FnMut(&Self) -> bool,
    ) -> Result<()> {
        let start = Instant::now();
        while !done(self) {
            ensure!(start.elapsed() < limit, "Timed out waiting for {what}");
            self.audit_frame()?;
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }
    fn audit_capture(&mut self, path: &Path) -> Result<()> {
        for _ in 0..4 {
            self.audit_frame()?;
        }
        self.renderer
            .as_mut()
            .context("Renderer not initialized")?
            .save_screenshot_with_ui(path)?;
        Ok(())
    }

    /// FERRITE_LLM_CHAT_UI_AUDIT=<dir>: enable the MCP service, open the chat
    /// window, wait for client detection, send one prompt per detected client
    /// (or FERRITE_LLM_CHAT_UI_PROVIDER only) and capture each reply.
    pub(super) fn audit_llm_chat_ui(&mut self, output: &Path) -> Result<()> {
        std::fs::create_dir_all(output)?;
        // Optional datasets, opened through the ordinary folder-open path so
        // the dataset tools have something to report.
        if let Some(folder) = std::env::var_os("FERRITE_LLM_CHAT_UI_DATASETS") {
            self.ensure_navigation_scene()?;
            for _ in 0..5 {
                self.renderer.as_mut().context("Renderer")?.render()?;
                self.sync_chart_layout();
            }
            self.begin_dataset_open(std::path::PathBuf::from(folder), true)?;
            let deadline = Instant::now() + Duration::from_secs(300);
            while self.dataset_open_discovery.is_some()
                || self.dataset_open_session.is_some()
                || self.loading_state.is_some()
            {
                ensure!(Instant::now() < deadline, "Dataset opening timed out");
                if self.loading_state.is_some() {
                    self.poll_loading();
                }
                self.poll_dataset_open();
                if let Some(error) = self.startup_error.take() {
                    anyhow::bail!(error);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            self.refresh_dataset_tree();
            for _ in 0..5 {
                self.renderer.as_mut().context("Renderer")?.render()?;
                if self.sync_chart_layout() {
                    self.update_view();
                }
            }
        }
        {
            let ui = &mut self.renderer.as_mut().context("Renderer")?.ui.ui_state;
            ui.s100_mcp.enabled = true;
            ui.s100_mcp.restart = true;
            ui.llm_chat.open = true;
        }
        self.audit_capture(&output.join("opened.png"))?;
        self.audit_wait("MCP endpoint", Duration::from_secs(60), |app| {
            app.renderer
                .as_ref()
                .is_some_and(|r| !r.ui.ui_state.s100_mcp.url.is_empty())
        })?;
        self.audit_wait("client detection", Duration::from_secs(90), |app| {
            app.renderer.as_ref().is_some_and(|r| {
                r.ui.ui_state.llm_chat.detected
                    && r.ui.ui_state.llm_chat.status != Status::Detecting
            })
        })?;
        self.audit_capture(&output.join("detected.png"))?;
        let providers: Vec<String> = self
            .renderer
            .as_ref()
            .unwrap()
            .ui
            .ui_state
            .llm_chat
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect();
        ensure!(!providers.is_empty(), "No LLM client detected");
        let only = std::env::var("FERRITE_LLM_CHAT_UI_PROVIDER").ok();
        // A UTF-8 file avoids console code pages for non-ASCII prompts.
        let from_file = std::env::var_os("FERRITE_LLM_CHAT_UI_PROMPT_FILE")
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_owned());
        let prompt = from_file
            .map_or_else(|| std::env::var("FERRITE_LLM_CHAT_UI_PROMPT"), Ok)
            .unwrap_or_else(|_| {
                "Use the ferrite_s100 tools to list the loaded datasets, then answer in one \
             short sentence."
                    .into()
            });
        let mut results = Vec::new();
        for id in providers
            .iter()
            .filter(|id| only.as_ref().is_none_or(|o| o == *id))
        {
            {
                let chat = &mut self.renderer.as_mut().unwrap().ui.ui_state.llm_chat;
                if chat.selected.as_deref() != Some(id) {
                    chat.selected = Some(id.clone());
                }
                chat.new_conversation = true;
            }
            self.audit_frame()?;
            {
                // The same request the Send button records.
                let chat = &mut self.renderer.as_mut().unwrap().ui.ui_state.llm_chat;
                chat.input = prompt.clone();
                chat.send = Some(std::mem::take(&mut chat.input).trim().to_owned());
            }
            self.audit_wait("reply start", Duration::from_secs(30), |app| {
                app.renderer
                    .as_ref()
                    .is_some_and(|r| r.ui.ui_state.llm_chat.running())
            })?;
            self.audit_capture(&output.join(format!("{id}-running.png")))?;
            self.audit_wait("reply", Duration::from_secs(300), |app| {
                app.renderer
                    .as_ref()
                    .is_some_and(|r| !r.ui.ui_state.llm_chat.running())
            })?;
            self.audit_capture(&output.join(format!("{id}-reply.png")))?;
            // The same request the reply's first "Show on Chart" button records.
            let first = self
                .renderer
                .as_ref()
                .unwrap()
                .ui
                .ui_state
                .llm_chat
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .and_then(|m| {
                    ferrite_ui::llm_chat_ui::find_positions(&m.text)
                        .into_iter()
                        .next()
                });
            if let Some((lat, lon, _)) = first {
                self.renderer
                    .as_mut()
                    .unwrap()
                    .ui
                    .ui_state
                    .llm_chat
                    .show_position = Some([lat, lon]);
                self.audit_frame()?;
                self.update_chat_marker();
                self.audit_capture(&output.join(format!("{id}-shown.png")))?;
            }
            let chat = &self.renderer.as_ref().unwrap().ui.ui_state.llm_chat;
            let reply = chat
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .map(|m| m.text.clone());
            results.push(serde_json::json!({
                "provider": id,
                "status": format!("{:?}", chat.status),
                "tools_attached": chat.tools_attached,
                "tools_note": chat.tools_note,
                "reply": reply,
                "notices": chat.messages.iter().filter(|m| m.role == Role::Notice)
                    .map(|m| m.text.clone()).collect::<Vec<_>>(),
            }));
            ensure!(reply.is_some(), "{id} produced no reply");
        }
        // Stop: a long reply is interrupted and no client process may remain
        // (process group kill on Unix, taskkill /T on Windows).
        let mut stop = serde_json::Value::Null;
        if let Some(id) = providers
            .iter()
            .find(|id| only.as_ref().is_none_or(|o| o == *id))
        {
            {
                let chat = &mut self.renderer.as_mut().unwrap().ui.ui_state.llm_chat;
                chat.selected = Some(id.clone());
                chat.new_conversation = true;
            }
            self.audit_frame()?;
            {
                let chat = &mut self.renderer.as_mut().unwrap().ui.ui_state.llm_chat;
                chat.send = Some(
                    "Write a detailed 3000-word history of nautical charting, step by step.".into(),
                );
            }
            self.audit_wait("long reply start", Duration::from_secs(30), |app| {
                app.renderer
                    .as_ref()
                    .is_some_and(|r| r.ui.ui_state.llm_chat.running())
            })?;
            // Let the client process tree start.
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(4) {
                self.audit_frame()?;
                std::thread::sleep(Duration::from_millis(100));
            }
            let before = client_processes();
            // The same request the Stop button records.
            self.renderer.as_mut().unwrap().ui.ui_state.llm_chat.stop = true;
            self.audit_wait("stop", Duration::from_secs(20), |app| {
                app.renderer
                    .as_ref()
                    .is_some_and(|r| !r.ui.ui_state.llm_chat.running())
            })?;
            let settled = Instant::now();
            while settled.elapsed() < Duration::from_secs(3) {
                self.audit_frame()?;
                std::thread::sleep(Duration::from_millis(100));
            }
            let after = client_processes();
            self.audit_capture(&output.join(format!("{id}-stopped.png")))?;
            ensure!(before > 0, "No client process was observed before Stop");
            ensure!(after == 0, "{after} client processes remained after Stop");
            stop = serde_json::json!({"provider": id, "processes_before": before, "processes_after": after});
        }
        std::fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"providers": providers, "turns": results, "stop": stop}),
            )?,
        )?;
        // Turning the service off must stop clients and close the session.
        self.renderer.as_mut().unwrap().ui.ui_state.s100_mcp.enabled = false;
        self.renderer.as_mut().unwrap().ui.ui_state.s100_mcp.restart = true;
        self.audit_frame()?;
        self.audit_capture(&output.join("service-off.png"))?;
        Ok(())
    }

    /// FERRITE_S421_EDIT_COPY_AUDIT=<dir> with FERRITE_S421_EDIT_COPY_INPUT=<gml>:
    /// import a route, press the panel's Edit Copy action, edit the copy,
    /// export it and reload it, capturing the panel at each step.
    pub(super) fn audit_s421_edit_copy(&mut self, output: &Path, input: &Path) -> Result<()> {
        std::fs::create_dir_all(output)?;
        let original = std::fs::read(input)?;
        let imported = self.native_s421.import_path(input, false)?;
        if !self.native_s421.panel_visible() {
            self.native_s421.toggle_panel();
        }
        self.audit_s421_publish_ui();
        self.audit_capture(&output.join("imported.png"))?;
        // The same event JSON the panel's "Edit Copy" button sends.
        self.handle_native_s421_ui(r#"{"type":"CopyAsAuthored"}"#)?;
        self.audit_s421_publish_ui();
        self.audit_capture(&output.join("copied.png"))?;
        let ui: serde_json::Value = serde_json::from_str(self.native_s421.ui_json())?;
        let waypoint = ui["waypoints"][1]["id"]
            .as_u64()
            .context("Copy has no second waypoint")?;
        self.handle_native_s421_ui(
            &serde_json::json!({"type":"SetTurnRadius","id":waypoint,"radius_nm":"0.25"})
                .to_string(),
        )?;
        let saved = output.join("edited.gml");
        self.native_s421.export_path(&saved)?;
        let reloaded = self.native_s421.import_path(&saved, false)?;
        self.audit_s421_publish_ui();
        self.audit_capture(&output.join("reloaded.png"))?;
        ensure!(
            std::fs::read(input)? == original,
            "Original route file changed"
        );
        std::fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "imported_route": imported,
                "reloaded_route": reloaded,
                "saved_bytes": std::fs::metadata(&saved)?.len(),
                "ui": serde_json::from_str::<serde_json::Value>(self.native_s421.ui_json())?,
            }))?,
        )?;
        Ok(())
    }
    fn audit_s421_publish_ui(&mut self) {
        let json = self.native_s421.ui_json().to_owned();
        let visible = self.native_s421.panel_visible();
        if let Some(renderer) = &mut self.renderer {
            renderer
                .ui
                .set_plugin_buttons(vec![ferrite_ui::PluginButton {
                    plugin_id: super::native_s421::PROVIDER_ID.into(),
                    label: "S-421 Routes".into(),
                    tooltip: None,
                    active: visible,
                }]);
            renderer
                .ui
                .set_plugin_ui_data(vec![(super::native_s421::PROVIDER_ID.into(), json)]);
        }
    }
}

/// Live descendants of this application process (the chat's clients and
/// anything they started), from the operating system's process table.
fn client_processes() -> usize {
    #[cfg(windows)]
    let listing = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_Process | ForEach-Object { \"$($_.ProcessId) $($_.ParentProcessId) $($_.Name)\" }",
        ])
        .output();
    #[cfg(not(windows))]
    let listing = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,comm="])
        .output();
    let Ok(listing) = listing else {
        return 0;
    };
    let text = String::from_utf8_lossy(&listing.stdout);
    let rows: Vec<(u32, u32, String)> = text
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((
                it.next()?.parse().ok()?,
                it.next()?.parse().ok()?,
                it.collect::<Vec<_>>().join(" "),
            ))
        })
        .collect();
    let mut tree = vec![std::process::id()];
    let mut count = 0;
    while let Some(parent) = tree.pop() {
        for (pid, ppid, name) in &rows {
            // The listing command itself is a child too.
            let lister = name.contains("powershell") || name.ends_with("ps");
            if *ppid == parent && *pid != parent && !lister {
                count += 1;
                tree.push(*pid);
            }
        }
    }
    count
}
