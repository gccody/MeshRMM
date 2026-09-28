//! The toolbar above the video, drawn by the viewer: see [`crate::toolbar`]
//! for its items, layout and icons. This view paints them, tracks the mouse,
//! shows their menus and tooltips, and moves the window from the empty space
//! between them. Its owner acts on the clicks.

use std::cell::Cell;

use super::app::RemoteView;
use super::*;
use crate::toolbar::{self, Command, Item, Layout, MenuEntry, Paint, Rect, Segment};
use objc2::AnyThread;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSBezierPath, NSFontAttributeName, NSFontWeightMedium, NSForegroundColorAttributeName,
    NSLineCapStyle, NSLineJoinStyle, NSMenu, NSMenuItem, NSStringDrawing, NSTrackingArea,
    NSTrackingAreaOptions,
};
use objc2_foundation::NSDictionary;

/// Leaves room for the window buttons, which an empty compact toolbar
/// centers in the 40-point title bar.
const LEADING_INSET: f64 = 84.0;

pub(super) struct ToolbarViewIvars {
    owner: objc2::rc::Weak<RemoteView>,
    state: RefCell<toolbar::State>,
    items: RefCell<Vec<Item>>,
    layout: RefCell<Layout>,
    hovered: Cell<Option<usize>>,
    pressed: Cell<Option<usize>>,
    /// The commands of the open menu, by item tag.
    menu: RefCell<Vec<Option<Command>>>,
    /// Tooltip owners: AppKit asks each for its description, and does not
    /// keep them alive.
    tooltips: RefCell<Vec<Retained<NSString>>>,
    font: Retained<NSFont>,
}

define_class!(
    // Safety: NSView is designed for subclassing; the toolbar stays on the
    // AppKit main thread and owns no resources requiring Drop.
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ToolbarViewIvars]
    pub(super) struct ToolbarView;

    unsafe impl NSObjectProtocol for ToolbarView {}

    impl ToolbarView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        /// The toolbar moves the window itself, only from its empty space.
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
            self.relayout();
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.track(Some(self.point(event)));
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, event: &NSEvent) {
            self.track(Some(self.point(event)));
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.track(None);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let (x, y) = self.point(event);
            let hit = self.ivars().layout.borrow().hit(x, y);
            let Some(index) = hit else {
                if let Some(window) = self.window() {
                    if event.clickCount() == 2 {
                        window.performZoom(None);
                    } else {
                        window.performWindowDragWithEvent(event);
                    }
                }
                return;
            };
            let Some(item) = self.ivars().items.borrow().get(index).cloned() else {
                return;
            };
            if !item.enabled {
                return;
            }
            self.ivars().pressed.set(Some(index));
            self.setNeedsDisplay(true);
            if item.menu {
                // The menu tracks the mouse until it closes.
                self.fire(index);
                self.ivars().pressed.set(None);
                self.track_current_location();
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if self.ivars().pressed.get().is_some() {
                self.track(Some(self.point(event)));
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let Some(pressed) = self.ivars().pressed.take() else {
                return;
            };
            let (x, y) = self.point(event);
            let hit = self.ivars().layout.borrow().hit(x, y);
            self.setNeedsDisplay(true);
            if hit == Some(pressed) {
                self.fire(pressed);
            }
        }

        // Clicks on the toolbar never reach the remote computer.
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, _event: &NSEvent) {}

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, _event: &NSEvent) {}

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, _event: &NSEvent) {}

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, _event: &NSEvent) {}

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, _event: &NSEvent) {}

        #[unsafe(method(toolbarMenuCommand:))]
        fn toolbar_menu_command(&self, sender: &NSMenuItem) {
            let command = usize::try_from(sender.tag())
                .ok()
                .and_then(|index| self.ivars().menu.borrow().get(index).copied().flatten());
            if let (Some(command), Some(owner)) = (command, self.ivars().owner.load()) {
                owner.toolbar_command(command);
            }
        }
    }
);

impl ToolbarView {
    pub(super) fn new(mtm: MainThreadMarker, frame: NSRect, owner: &RemoteView) -> Retained<Self> {
        let font =
            NSFont::systemFontOfSize_weight(toolbar::FONT_SIZE, unsafe { NSFontWeightMedium });
        let this = Self::alloc(mtm).set_ivars(ToolbarViewIvars {
            owner: objc2::rc::Weak::new(owner),
            state: RefCell::new(toolbar::State::default()),
            items: RefCell::new(Vec::new()),
            layout: RefCell::new(Layout::default()),
            hovered: Cell::new(None),
            pressed: Cell::new(None),
            menu: RefCell::new(Vec::new()),
            tooltips: RefCell::new(Vec::new()),
            font,
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        // Safety: the tracking area's owner is this view, which outlives it.
        let tracking = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&this),
                None,
            )
        };
        this.addTrackingArea(&tracking);
        this
    }

    /// Shows `state`, redrawing only when an item changed.
    pub(super) fn set_state(&self, state: toolbar::State) {
        if *self.ivars().state.borrow() == state {
            return;
        }
        let items = toolbar::items(&state);
        self.ivars().state.replace(state);
        if *self.ivars().items.borrow() != items {
            self.ivars().items.replace(items);
            self.relayout();
        }
    }

    pub(super) fn state(&self) -> toolbar::State {
        self.ivars().state.borrow().clone()
    }

    /// Opens `entries` under the item at `rect`. The owner receives the
    /// chosen command.
    pub(super) fn show_menu(&self, entries: &[MenuEntry], rect: Rect) {
        let menu = NSMenu::new(self.mtm());
        menu.setAutoenablesItems(false);
        for (index, entry) in entries.iter().enumerate() {
            match entry {
                MenuEntry::Separator => menu.addItem(&NSMenuItem::separatorItem(self.mtm())),
                MenuEntry::Item {
                    label,
                    checked,
                    enabled,
                    command,
                } => {
                    // Safety: the target outlives the menu, which is modal.
                    let item = unsafe {
                        NSMenuItem::initWithTitle_action_keyEquivalent(
                            NSMenuItem::alloc(self.mtm()),
                            &NSString::from_str(label),
                            Some(sel!(toolbarMenuCommand:)),
                            &NSString::new(),
                        )
                    };
                    unsafe { item.setTarget(Some(self)) };
                    item.setTag(index as isize);
                    item.setEnabled(*enabled && command.is_some());
                    item.setState(isize::from(*checked));
                    menu.addItem(&item);
                }
            }
        }
        self.ivars().menu.replace(toolbar::commands(entries));
        self.pop_up(&menu, rect);
    }

    /// Opens a menu under the item at `rect`.
    pub(super) fn pop_up(&self, menu: &NSMenu, rect: Rect) {
        menu.popUpMenuPositioningItem_atLocation_inView(
            None,
            NSPoint::new(rect.x, rect.bottom() + 4.0),
            Some(self),
        );
    }

    fn point(&self, event: &NSEvent) -> (f64, f64) {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        (point.x, point.y)
    }

    fn track(&self, point: Option<(f64, f64)>) {
        let hovered = point.and_then(|(x, y)| self.ivars().layout.borrow().hit(x, y));
        if self.ivars().hovered.replace(hovered) != hovered {
            self.setNeedsDisplay(true);
        }
    }

    /// A menu consumes the mouse-up and any movement while it is open.
    fn track_current_location(&self) {
        let point = self.window().map(|window| {
            let point =
                self.convertPoint_fromView(window.mouseLocationOutsideOfEventStream(), None);
            (point.x, point.y)
        });
        self.track(point);
        self.setNeedsDisplay(true);
    }

    fn fire(&self, index: usize) {
        let action = self
            .ivars()
            .items
            .borrow()
            .get(index)
            .map(|item| item.action);
        let rect = self.ivars().layout.borrow().rects.get(index).copied();
        if let (Some(action), Some(rect), Some(owner)) = (action, rect, self.ivars().owner.load()) {
            owner.toolbar_action(action, rect);
        }
    }

    fn attributes(
        &self,
        color: toolbar::Color,
    ) -> Retained<NSDictionary<objc2_foundation::NSAttributedStringKey, AnyObject>> {
        let color = ns_color(color);
        let keys = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
        let objects: [&AnyObject; 2] = [self.ivars().font.as_ref(), color.as_ref()];
        NSDictionary::from_slices(&keys, &objects)
    }

    fn measure(&self, text: &str) -> f64 {
        let attributes = self.attributes(toolbar::BACKGROUND);
        unsafe { NSString::from_str(text).sizeWithAttributes(Some(&attributes)) }.width
    }

    fn relayout(&self) {
        let width = self.bounds().size.width;
        let layout = toolbar::layout(
            &self.ivars().items.borrow(),
            width,
            LEADING_INSET,
            &|text| self.measure(text),
        );
        self.removeAllToolTips();
        let mut tooltips = Vec::new();
        for (item, rect) in self.ivars().items.borrow().iter().zip(&layout.rects) {
            let tooltip = NSString::from_str(&item.tooltip);
            // Safety: `tooltips` keeps the owner alive until the next layout
            // removes its tooltip.
            unsafe {
                self.addToolTipRect_owner_userData(
                    ns_rect(*rect),
                    tooltip.as_ref(),
                    ptr::null_mut(),
                )
            };
            tooltips.push(tooltip);
        }
        self.ivars().tooltips.replace(tooltips);
        self.ivars().layout.replace(layout);
        self.setNeedsDisplay(true);
    }

    fn draw(&self) {
        let bounds = self.bounds();
        let width = bounds.size.width;
        fill_rect(
            Rect::new(0.0, 0.0, width, toolbar::HEIGHT),
            toolbar::BACKGROUND,
        );
        fill_rect(
            Rect::new(0.0, toolbar::HEIGHT - 1.0, width, 1.0),
            toolbar::BORDER,
        );
        let items = self.ivars().items.borrow();
        let layout = self.ivars().layout.borrow();
        for x in &layout.separators {
            fill_rect(Rect::new(x - 0.5, 12.0, 1.0, 16.0), toolbar::SEPARATOR);
        }
        let hovered = self.ivars().hovered.get();
        let pressed = self.ivars().pressed.get();
        for (index, (item, rect)) in items.iter().zip(&layout.rects).enumerate() {
            let style = toolbar::style(item, hovered == Some(index), pressed == Some(index));
            if let Some(background) = style.background {
                fill_path(&toolbar::rounded_rect(*rect, style.radius), background);
            }
            let text = item
                .label
                .as_deref()
                .map(|label| (label, self.measure(label)));
            let parts = toolbar::parts(item, *rect, text.map_or(0.0, |(_, width)| width));
            for shape in toolbar::icon(item.icon) {
                paint(&toolbar::place(&shape, parts.icon), style.foreground);
            }
            if let (Some((label, _)), Some(area)) = (text, parts.label) {
                let attributes = self.attributes(style.label);
                let label = NSString::from_str(label);
                let size = unsafe { label.sizeWithAttributes(Some(&attributes)) };
                unsafe {
                    label.drawAtPoint_withAttributes(
                        NSPoint::new(area.x, area.y + (area.height - size.height) / 2.0),
                        Some(&attributes),
                    )
                };
            }
            if let Some(chevron) = parts.chevron {
                for shape in toolbar::icon(toolbar::Icon::Chevron) {
                    paint(&toolbar::place(&shape, chevron), style.foreground);
                }
            }
            if let (Some((x, y)), Some(color)) = (parts.badge, style.badge) {
                let ring = style.badge_radius + 1.5;
                fill_oval(x, y, ring, style.badge_ring);
                fill_oval(x, y, style.badge_radius, color);
            }
        }
    }
}

fn ns_rect(rect: Rect) -> NSRect {
    NSRect::new(
        NSPoint::new(rect.x, rect.y),
        NSSize::new(rect.width, rect.height),
    )
}

fn ns_color(color: toolbar::Color) -> Retained<NSColor> {
    let toolbar::Color(red, green, blue) = color;
    NSColor::colorWithSRGBRed_green_blue_alpha(
        f64::from(red) / 255.0,
        f64::from(green) / 255.0,
        f64::from(blue) / 255.0,
        1.0,
    )
}

fn fill_rect(rect: Rect, color: toolbar::Color) {
    ns_color(color).setFill();
    NSBezierPath::fillRect(ns_rect(rect));
}

fn fill_oval(x: f64, y: f64, radius: f64, color: toolbar::Color) {
    ns_color(color).setFill();
    NSBezierPath::bezierPathWithOvalInRect(ns_rect(Rect::new(
        x - radius,
        y - radius,
        2.0 * radius,
        2.0 * radius,
    )))
    .fill();
}

fn bezier(segments: &[Segment]) -> Retained<NSBezierPath> {
    let path = NSBezierPath::bezierPath();
    for segment in segments {
        match *segment {
            Segment::Move(x, y) => path.moveToPoint(NSPoint::new(x, y)),
            Segment::Line(x, y) => path.lineToPoint(NSPoint::new(x, y)),
            Segment::Cubic(ax, ay, bx, by, x, y) => path.curveToPoint_controlPoint1_controlPoint2(
                NSPoint::new(x, y),
                NSPoint::new(ax, ay),
                NSPoint::new(bx, by),
            ),
            Segment::Close => path.closePath(),
        }
    }
    path
}

fn fill_path(segments: &[Segment], color: toolbar::Color) {
    ns_color(color).setFill();
    bezier(segments).fill();
}

fn paint((segments, paint): &(Vec<Segment>, Paint), color: toolbar::Color) {
    let path = bezier(segments);
    let color = ns_color(color);
    match *paint {
        Paint::Fill => {
            color.setFill();
            path.fill();
        }
        Paint::Stroke(width) => {
            color.setStroke();
            path.setLineWidth(width);
            path.setLineCapStyle(NSLineCapStyle::Round);
            path.setLineJoinStyle(NSLineJoinStyle::Round);
            path.stroke();
        }
    }
}
