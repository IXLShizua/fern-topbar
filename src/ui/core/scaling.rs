//! Layout-aware UI scaling, independent of the monitor's HiDPI scale.

use relm4::gtk::{self, glib, prelude::*, subclass::prelude::*};

/// Validated application zoom shared by panel, menus and notification windows.
/// Values from 0.25 to 4 include both integer and fractional factors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiScale(f64);

impl Default for UiScale {
    /// Preserves the existing UI size and the compositor's own output scale.
    fn default() -> Self {
        Self(1.0)
    }
}

impl UiScale {
    /// Validates a finite zoom factor between 0.25 and 4, inclusive.
    pub fn new(factor: f64) -> Result<Self, String> {
        if !factor.is_finite() || !(0.25..=4.0).contains(&factor) {
            return Err("scale must be a finite number between 0.25 and 4".into());
        }

        Ok(Self(factor))
    }

    /// Returns the multiplier applied to unscaled widget coordinates.
    pub fn factor(self) -> f64 {
        self.0
    }

    /// Converts a nonnegative widget size to application pixels, rounding up.
    /// Negative GTK sentinel values, such as an unspecified baseline, survive.
    pub fn pixels(self, size: i32) -> i32 {
        if size < 0 {
            size
        } else {
            (f64::from(size) * self.0).ceil() as i32
        }
    }

    /// Converts an available pixel budget to widget units without exceeding it.
    /// Negative GTK sentinel values are preserved.
    pub fn units(self, size: i32) -> i32 {
        if size < 0 {
            size
        } else {
            (f64::from(size) / self.0).floor() as i32
        }
    }

    /// Keeps native popover shadows and clipping radii aligned with scaled content.
    /// Reuses the compiled theme's declarations instead of duplicating its tokens.
    pub fn stylesheet(self, stylesheet: &str) -> String {
        if self == Self::default() {
            return stylesheet.to_owned();
        }

        let surface = stylesheet
            .split_once("popover.topbar-menu > contents {")
            .and_then(|(_, rest)| rest.split_once('}'))
            .map(|(surface, _)| surface)
            .expect("the panel theme defines the standard popover surface");

        let mut scaled = stylesheet.to_owned();
        scaled.push_str(
            "\npopover.topbar-menu.topbar-scaled-popover > contents, \
             popover.topbar-mixer.topbar-scaled-popover > contents {\n",
        );

        for declaration in surface.lines().map(str::trim) {
            if declaration.starts_with("box-shadow:") || declaration.starts_with("border-radius:") {
                for token in declaration.split_whitespace() {
                    if let Some((number, suffix)) = token.split_once("px")
                        && let Ok(number) = number.parse::<f64>()
                    {
                        scaled.push_str(&format!("{}px{suffix} ", number * self.0));
                    } else {
                        scaled.push_str(token);
                        scaled.push(' ');
                    }
                }

                scaled.push('\n');
            }
        }

        scaled.push_str("}\n");

        scaled
    }

    /// Wraps an unparented subtree in a layout-aware scaling container.
    /// At the default factor, returns the original widget unchanged.
    pub fn wrap(self, child: &impl IsA<gtk::Widget>) -> gtk::Widget {
        if self == Self::default() {
            return child.as_ref().clone();
        }

        let widget: Scaled = glib::Object::new();
        widget.imp().scale.set(self);
        child.set_parent(&widget);

        widget.upcast()
    }

    /// Finds the closest scaling container around a widget, or returns 1.
    /// Useful for converting native monitor budgets to menu/content coordinates.
    pub fn for_widget(widget: &impl IsA<gtk::Widget>) -> Self {
        let mut ancestor = Some(widget.as_ref().clone());

        while let Some(widget) = ancestor {
            if let Some(scaled) = widget.downcast_ref::<Scaled>() {
                return scaled.imp().scale.get();
            }

            ancestor = widget.parent();
        }

        Self::default()
    }

    /// Scales an unmapped window's child and initial default size exactly once.
    pub fn configure_window(self, window: &impl IsA<gtk::Window>) {
        if self == Self::default() {
            return;
        }

        let Some(child) = window.child() else {
            return;
        };

        if child.is::<Scaled>() {
            return;
        }

        window.set_child(None::<&gtk::Widget>);
        window.set_child(Some(&self.wrap(&child)));

        let (width, height) = window.default_size();

        window.set_default_size(self.pixels(width), self.pixels(height));
    }

    /// Scales a popover's content and surface decoration before its first map.
    /// Native popup positioning stays under GTK's control; its offset scales too.
    pub fn configure_popover(self, popover: &gtk::Popover) {
        if self == Self::default() || popover.has_css_class("topbar-scaled-popover") {
            return;
        }

        let Some(child) = popover.child() else {
            return;
        };
        let surface = gtk::Box::new(gtk::Orientation::Vertical, 0);

        for class in popover.css_classes() {
            surface.add_css_class(&class);
        }

        surface.add_css_class("topbar-scaled-surface");
        popover.set_child(None::<&gtk::Widget>);
        surface.append(&child);
        popover.set_child(Some(&self.wrap(&surface)));
        popover.add_css_class("topbar-scaled-popover");

        let (offset_x, offset_y) = popover.offset();

        popover.set_offset(
            (f64::from(offset_x) * self.0).round() as i32,
            (f64::from(offset_y) * self.0).round() as i32,
        );
    }
}

glib::wrapper! {
    /// Internal single-child widget applying a GSK transform during allocation.
    pub struct Scaled(ObjectSubclass<imp::Scaled>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

mod imp {
    use super::Scaled as ScaledWidget;
    use super::*;
    use std::cell::Cell;

    /// GTK implementation state for a single scaled child.
    #[derive(Default)]
    pub struct Scaled {
        /// The validated factor used for layout and allocation transforms.
        pub scale: Cell<UiScale>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Scaled {
        const NAME: &'static str = "FernTopbarScaled";
        type Type = ScaledWidget;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Scaled {
        /// Releases the parent-child relationship when the container is disposed.
        fn dispose(&self) {
            if let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for Scaled {
        /// Preserves keyboard navigation inside the scaled subtree.
        fn focus(&self, direction: gtk::DirectionType) -> bool {
            self.obj()
                .first_child()
                .is_some_and(|child| child.child_focus(direction))
        }

        /// Delegates wrapping direction to the child.
        fn request_mode(&self) -> gtk::SizeRequestMode {
            self.obj()
                .first_child()
                .map_or(gtk::SizeRequestMode::ConstantSize, |child| {
                    child.request_mode()
                })
        }

        /// Measures in child coordinates and returns scaled sizes and baselines.
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let Some(child) = self.obj().first_child() else {
                return (0, 0, -1, -1);
            };
            let scale = self.scale.get();
            let (minimum, natural, min_baseline, natural_baseline) =
                child.measure(orientation, scale.units(for_size));

            (
                scale.pixels(minimum),
                scale.pixels(natural),
                scale.pixels(min_baseline),
                scale.pixels(natural_baseline),
            )
        }

        /// Allocates unscaled child space and lets GTK transform rendering/picking.
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(child) = self.obj().first_child() {
                let scale = self.scale.get();
                let factor = scale.factor() as f32;

                child.allocate(
                    scale.units(width),
                    scale.units(height),
                    scale.units(baseline),
                    Some(gtk::gsk::Transform::new().scale(factor, factor)),
                );
            }
        }

        /// Draws the child through its allocation transform, without raster resizing.
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            if let Some(child) = self.obj().first_child() {
                self.obj().snapshot_child(&child, snapshot);
            }
        }

        /// Keeps expansion behavior consistent with the wrapped subtree.
        fn compute_expand(&self, hexpand: &mut bool, vexpand: &mut bool) {
            if let Some(child) = self.obj().first_child() {
                *hexpand = child.compute_expand(gtk::Orientation::Horizontal);
                *vexpand = child.compute_expand(gtk::Orientation::Vertical);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_chrome_uses_the_same_scale_and_the_existing_theme() {
        let theme = include_str!(concat!(env!("OUT_DIR"), "/styles.css"));

        assert_eq!(UiScale::default().stylesheet(theme), theme);

        let scaled = UiScale::new(1.5).unwrap().stylesheet(theme);
        let override_rule = scaled.strip_prefix(theme).unwrap();

        assert!(override_rule.contains("border-radius: 30px;"));
        assert!(override_rule.contains("box-shadow: 0 12px 33px rgba("));
        assert!(!override_rule.contains("padding:"));
    }

    #[test]
    fn pixel_budgets_round_down_and_gtk_sentinels_survive() {
        for factor in [0.25, 0.75, 1.0, 1.25, 1.5, 2.0, 4.0] {
            let scale = UiScale::new(factor).unwrap();

            assert_eq!(scale.pixels(-1), -1);
            assert_eq!(scale.units(-1), -1);

            for budget in [34, 121, 539, 540, 721] {
                assert!(scale.pixels(scale.units(budget)) <= budget);
            }
        }
    }

    #[gtk::test]
    fn scaled_layout_and_pointer_picking_use_the_same_transform() {
        for factor in [0.75, 1.25, 1.5, 2.0] {
            let scale = UiScale::new(factor).unwrap();
            let button = gtk::Button::with_label("Scaled control");
            button.set_size_request(120, 48);

            let horizontal = button.measure(gtk::Orientation::Horizontal, -1);
            let vertical = button.measure(gtk::Orientation::Vertical, horizontal.1);
            let wrapper = scale.wrap(&button);

            assert_eq!(UiScale::for_widget(&button), scale);
            assert_eq!(
                wrapper.measure(gtk::Orientation::Horizontal, -1).1,
                scale.pixels(horizontal.1)
            );

            let width = scale.pixels(horizontal.1);
            let height = scale.pixels(vertical.1);
            let window = gtk::Window::new();
            window.set_decorated(false);
            window.set_child(Some(&wrapper));
            window.set_default_size(width, height);
            window.present();

            let main_loop = glib::MainLoop::new(None, false);
            let quit = main_loop.clone();

            glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
                quit.quit()
            });
            main_loop.run();

            let bounds = button.compute_bounds(&wrapper).unwrap();
            let unscaled_bounds = button.compute_bounds(&button).unwrap();

            assert!((bounds.width() - unscaled_bounds.width() * factor as f32).abs() <= 1.0);

            let picked = wrapper
                .pick(
                    f64::from(bounds.width()) / 2.0,
                    f64::from(bounds.height()) / 2.0,
                    gtk::PickFlags::DEFAULT,
                )
                .unwrap();

            assert!(picked == button || picked.is_ancestor(&button));

            window.destroy();
        }
    }

    #[gtk::test]
    fn popover_content_and_chrome_scale_together_without_double_wrapping() {
        relm4::set_global_css(include_str!(concat!(env!("OUT_DIR"), "/styles.css")));

        let scale = UiScale::new(1.5).unwrap();
        let menu = gtk::Popover::new();
        menu.add_css_class("topbar-menu");
        menu.add_css_class("topbar-network-menu");
        menu.set_offset(0, 12);

        let content = gtk::Label::new(Some("Network"));

        menu.set_child(Some(&content));
        scale.configure_popover(&menu);

        let wrapper = menu.child().unwrap();
        let surface = wrapper.first_child().unwrap();

        assert!(surface.has_css_class("topbar-scaled-surface"));
        assert!(surface.has_css_class("topbar-network-menu"));

        let minimum = surface.measure(gtk::Orientation::Horizontal, -1).0;

        assert!(minimum >= 360);
        assert_eq!(
            wrapper.measure(gtk::Orientation::Horizontal, -1).0,
            scale.pixels(minimum)
        );
        assert_eq!(UiScale::for_widget(&content), scale);
        assert_eq!(menu.offset(), (0, 18));

        scale.configure_popover(&menu);

        assert_eq!(menu.child(), Some(wrapper));
        assert_eq!(menu.offset(), (0, 18));
    }
}
