//! External plugin types shared between the platform and the plugin SDK.
//!
//! External plugins are standalone binaries that Temps discovers, spawns, and
//! communicates with over Unix domain sockets. This module contains the shared
//! types used by both the plugin SDK (`temps-plugin-sdk`) and the
//! plugin management crate (`temps-external-plugins`).
//!
//! The actual lifecycle management, proxying, and API handlers live in the
//! `temps-external-plugins` crate.

pub mod channel;
pub mod manifest;

pub use channel::{
    ChannelError, ChannelErrorCode, ChannelEvent, ChannelMessage, ChannelRequest, ChannelResponse,
    DeploymentInfo, EnvironmentInfo, ProjectInfo, PLUGIN_CHANNEL_PATH,
};
pub use manifest::{
    HandshakeMessage, NavEntry, NavSection, PluginEvent, PluginManifest, PluginManifestBuilder,
    PluginReady, UiManifest, UiRoute, PLUGIN_EVENTS_PATH,
};
