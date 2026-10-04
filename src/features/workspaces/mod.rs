mod view;

use super::{FeatureDefinition, FeatureId, FeatureMountContext, MountedFeature};
use crate::{runtime::Task, ui::monitor::MonitorSubscription};
use relm4::gtk::prelude::*;
use relm4::{Component, ComponentController, Controller, gtk};

/// Describes availability and mounting without starting the feature.
pub fn definition() -> FeatureDefinition {
    FeatureDefinition {
        available: |services| services.availability.subscribe(FeatureId::Workspaces),
        mount: |context| Ok(mount(context)),
    }
}

/// Owns the component and every resource required by this mounted feature.
pub struct Mounted {
    controller: Controller<view::Workspaces>,
    // These guards release both subscriptions when the mounted feature is dropped.
    _forwarder: Task,
    _monitor_subscription: MonitorSubscription,
}

impl Mounted {
    pub fn widget(&self) -> &gtk::Widget {
        self.controller.widget().as_ref()
    }
}

fn mount(context: FeatureMountContext) -> MountedFeature {
    let mut updates = context.window_manager.subscribe_workspaces();
    let component = view::Workspaces::builder()
        .launch(view::WorkspacesInit {
            wm_commands: context.wm_commands,
        })
        .detach();

    let monitor_subscription = context
        .monitor_selection
        .subscribe(component.sender().clone(), |monitor| {
            view::Input::OutputChanged(monitor.connector().map(|output| output.to_string()))
        });

    let input = component.sender().clone();
    let forwarder = Task::spawn(async move {
        loop {
            let workspaces = updates.borrow_and_update().clone();

            if input
                .send(view::Input::WorkspacesChanged(workspaces))
                .is_err()
            {
                break;
            }

            if updates.changed().await.is_err() {
                break;
            }
        }
    });

    MountedFeature::Workspaces(Mounted {
        controller: component,
        _forwarder: forwarder,
        _monitor_subscription: monitor_subscription,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backend::wm::{Command, Workspace, state::WindowManagerState},
        features::{FeatureMountContext, FeatureServices},
        ui::{core::PopoverScope, monitor::MonitorSelection},
    };
    use std::time::{Duration, Instant};
    use view::{Input, Workspaces};

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let context = gtk::glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(2);

        loop {
            while context.pending() {
                context.iteration(false);
            }

            if ready() {
                return;
            }

            assert!(Instant::now() < deadline, "component update timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_for_titles(component: &Controller<Workspaces>, expected: &[&str]) {
        wait_until(|| {
            let mut titles = Vec::new();
            let mut child = component.widget().first_child().unwrap().first_child();

            while let Some(widget) = child {
                titles.push(widget.tooltip_text().unwrap().to_string());
                child = widget.next_sibling();
            }

            titles == expected
        });
    }

    #[gtk::test]
    fn monitor_changes_refilter_cached_workspaces_and_late_mounts_keep_current_data() {
        let monitor = gtk::gdk::Display::default()
            .unwrap()
            .monitors()
            .item(0)
            .unwrap()
            .downcast::<gtk::gdk::Monitor>()
            .unwrap();

        let output = monitor.connector().map(|output| output.to_string());
        let other_output = Some("test-other-monitor".into());
        let selection = MonitorSelection::default();
        selection.set(monitor.clone());

        let state = WindowManagerState::default();
        let mut workspaces = vec![
            Workspace {
                id: 7,
                index: 1,
                name: Some("Code".into()),
                output,
                active: true,
                urgent: false,
            },
            Workspace {
                id: 8,
                index: 2,
                name: Some("Chat".into()),
                output: other_output.clone(),
                active: false,
                urgent: true,
            },
        ];

        state.set_workspaces(workspaces.clone());

        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        let services = FeatureServices {
            window_manager: state.clone(),
            monitor_selection: selection.clone(),
            wm_commands: Some(commands),
            ..FeatureServices::default()
        };

        let mount = || {
            let context =
                FeatureMountContext::new(&services, PopoverScope::default(), Default::default());
            let MountedFeature::Workspaces(mounted) = super::mount(context) else {
                panic!("expected the workspaces feature");
            };

            mounted
        };

        let mounted = mount();
        let component = &mounted.controller;

        wait_for_titles(component, &["Code"]);

        // Changing the panel output needs no new WM event.
        let updates = state.subscribe_workspaces();

        component.emit(Input::OutputChanged(other_output));
        wait_for_titles(component, &["Chat"]);
        assert!(!updates.has_changed().unwrap());

        let button = component
            .widget()
            .first_child()
            .unwrap()
            .first_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();

        assert!(button.has_css_class("urgent"));
        button.emit_clicked();

        let mut command = None;

        wait_until(|| {
            command = received.try_recv().ok();

            command.is_some()
        });
        assert!(matches!(command, Some(Command::FocusWorkspace(8))));

        selection.set(monitor);
        wait_for_titles(component, &["Code"]);

        workspaces[0].name = Some("Editor".into());
        state.set_workspaces(workspaces);
        wait_for_titles(component, &["Editor"]);

        drop(mounted);

        let remounted = mount();

        wait_for_titles(&remounted.controller, &["Editor"]);
    }
}
