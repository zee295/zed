#[cfg(not(target_family = "wasm"))]
mod audio_input_output_setup;
#[cfg(not(target_family = "wasm"))]
mod audio_test_window;
#[cfg(not(target_family = "wasm"))]
mod edit_prediction_provider_setup;
mod external_agents_page;
mod feature_flags;
mod llm_providers_page;
#[cfg(not(target_family = "wasm"))]
mod mcp_servers_page;
mod sandbox_settings;
mod skill_creator;
mod skills_setup;
mod tool_permissions_setup;

pub(crate) use external_agents_page::{
    CustomAgentForm, render_add_agent_popover, render_external_agents_page,
};
pub(crate) use feature_flags::render_feature_flags_page;
pub(crate) use llm_providers_page::{
    LlmProviderForm, render_add_llm_provider_popover, render_llm_providers_page,
};

// --- Native-only pages (audio devices, edit-prediction providers, MCP servers
// via extension_host). On wasm these have lightweight stubs that render an
// "unavailable in the browser" note so the settings window still compiles and
// every other page works.
#[cfg(not(target_family = "wasm"))]
pub(crate) use audio_input_output_setup::{
    render_input_audio_device_dropdown, render_output_audio_device_dropdown,
};
#[cfg(not(target_family = "wasm"))]
pub(crate) use audio_test_window::open_audio_test_window;
#[cfg(not(target_family = "wasm"))]
pub(crate) use edit_prediction_provider_setup::render_edit_prediction_setup_page;
#[cfg(not(target_family = "wasm"))]
pub(crate) use mcp_servers_page::{
    McpServerForm, render_add_server_popover, render_mcp_servers_page,
};

#[cfg(target_family = "wasm")]
mod wasm_stubs {
    use gpui::{AnyElement, App, Context, ScrollHandle, Window, div, prelude::*};
    use ui::IntoElement as _;

    use crate::{SettingField, SettingsUiFile, SettingsWindow};
    use settings::{AudioInputDeviceName, AudioOutputDeviceName};

    fn unavailable() -> AnyElement {
        div()
            .child("Audio device selection is not available in the browser build.")
            .into_any_element()
    }

    pub(crate) fn render_input_audio_device_dropdown(
        _field: SettingField<AudioInputDeviceName>,
        _file: SettingsUiFile,
        _metadata: Option<&crate::SettingsFieldMetadata>,
        _title: &'static str,
        _description: &'static str,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        unavailable()
    }

    pub(crate) fn render_output_audio_device_dropdown(
        _field: SettingField<AudioOutputDeviceName>,
        _file: SettingsUiFile,
        _metadata: Option<&crate::SettingsFieldMetadata>,
        _title: &'static str,
        _description: &'static str,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        unavailable()
    }

    pub(crate) fn open_audio_test_window(_window: &mut Window, _cx: &mut App) {}

    pub(crate) fn render_edit_prediction_setup_page(
        _settings_window: &SettingsWindow,
        _scroll_handle: &ScrollHandle,
        _window: &mut Window,
        _cx: &mut Context<SettingsWindow>,
    ) -> AnyElement {
        div()
            .child("Edit prediction providers are not available in the browser build.")
            .into_any_element()
    }

    pub(crate) fn render_mcp_servers_page(
        _settings_window: &SettingsWindow,
        _scroll_handle: &ScrollHandle,
        _window: &mut Window,
        _cx: &mut Context<SettingsWindow>,
    ) -> AnyElement {
        div()
            .child("MCP server configuration is not available in the browser build.")
            .into_any_element()
    }

    pub(crate) fn render_add_server_popover(
        _settings_window: &SettingsWindow,
        _window: &mut Window,
        _cx: &mut Context<SettingsWindow>,
    ) -> AnyElement {
        div().into_any_element()
    }

    #[derive(Default)]
    pub(crate) struct McpServerForm;
}

pub(crate) use sandbox_settings::render_sandbox_settings_page;
pub use skill_creator::SkillCreatorOpenMode;
pub(crate) use skill_creator::{
    SkillCreatorEvent, SkillCreatorPage, render_skill_creator_page, skill_url_from_clipboard,
};
#[cfg(test)]
pub(crate) use skills_setup::displayed_skills;
pub(crate) use skills_setup::render_skills_setup_page;
pub(crate) use tool_permissions_setup::render_tool_permissions_setup_page;
#[cfg(target_family = "wasm")]
pub(crate) use wasm_stubs::{
    McpServerForm, open_audio_test_window, render_add_server_popover,
    render_edit_prediction_setup_page, render_input_audio_device_dropdown, render_mcp_servers_page,
    render_output_audio_device_dropdown,
};

pub use tool_permissions_setup::{
    render_copy_path_tool_config, render_create_directory_tool_config,
    render_delete_path_tool_config, render_edit_file_tool_config, render_fetch_tool_config,
    render_move_path_tool_config, render_skill_tool_config, render_terminal_tool_config,
    render_web_search_tool_config, render_write_file_tool_config,
};
