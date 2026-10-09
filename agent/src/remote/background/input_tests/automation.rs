//! Session 0 checks of controls only UI Automation finds: the Firewall
//! wizard's and Resource Monitor's.
use super::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0_0, VT_I4};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Controls::LVM_GETITEMCOUNT;
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;

/// A control UI Automation found, with what the tests match it on.
struct Element {
    element: IUIAutomationElement,
    name: String,
    id: String,
    rect: RECT,
}

impl Element {
    /// Where the control is now; it may have moved since it was found.
    fn bounds(&self) -> RECT {
        unsafe { self.element.CurrentBoundingRectangle() }.unwrap_or_default()
    }

    fn click(&self, workspace: &mut Workspace) -> anyhow::Result<()> {
        let rect = self.bounds();
        click_at(
            workspace,
            (rect.left + rect.right) / 2,
            (rect.top + rect.bottom) / 2,
        )
    }

    fn selected(&self) -> bool {
        unsafe {
            self.element
                .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                    UIA_SelectionItemPatternId,
                )
                .and_then(|pattern| pattern.CurrentIsSelected())
                .is_ok_and(|selected| selected.as_bool())
        }
    }
}

/// UI Automation finds controls that aren't windows, such as Resource
/// Monitor's DirectUI buttons, and reads state that window messages don't
/// report, such as whether a WinForms radio button is selected.
struct Automation(IUIAutomation);

impl Automation {
    fn new() -> anyhow::Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            Ok(Self(CoCreateInstance(
                &CUIAutomation,
                None,
                CLSCTX_INPROC_SERVER,
            )?))
        }
    }

    /// The controls of type `kind` under `window`. Controls that go away
    /// while they're read are left out.
    fn find(&self, window: HWND, kind: UIA_CONTROLTYPE_ID) -> anyhow::Result<Vec<Element>> {
        unsafe {
            let mut value = VARIANT::default();
            value.Anonymous.Anonymous = std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                ..Default::default()
            });
            (*value.Anonymous.Anonymous).Anonymous.lVal = kind.0;
            let condition = self
                .0
                .CreatePropertyCondition(UIA_ControlTypePropertyId, &value)?;
            let found = self
                .0
                .ElementFromHandle(window)?
                .FindAll(TreeScope_Descendants, &condition)?;
            Ok((0..found.Length()?)
                .filter_map(|index| {
                    let element = found.GetElement(index).ok()?;
                    Some(Element {
                        name: element.CurrentName().ok()?.to_string(),
                        id: element.CurrentAutomationId().ok()?.to_string(),
                        rect: element.CurrentBoundingRectangle().ok()?,
                        element,
                    })
                })
                .collect())
        }
    }

    /// Waits up to 20 seconds for a control of type `kind` under `window`
    /// that satisfies `accept`.
    fn wait(
        &self,
        workspace: &mut Workspace,
        what: &str,
        window: HWND,
        kind: UIA_CONTROLTYPE_ID,
        accept: &dyn Fn(&Element) -> bool,
    ) -> anyhow::Result<Element> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(element) = self.find(window, kind)?.into_iter().find(accept) {
                return Ok(element);
            }
            if Instant::now() >= deadline {
                let _ = proof(&format!(
                    "background-{}-failure.bmp",
                    what.replace(' ', "-")
                ));
                anyhow::bail!("timed out waiting for {what}");
            }
            settle(workspace, 100);
        }
    }
}

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn firewall_wizard_takes_clicks_and_cancels() -> anyhow::Result<()> {
    in_workspace(|workspace| {
        let automation = Automation::new()?;
        workspace.launch(pin("mmc.exe", "wf.msc"))?;
        let frame = wait_for_job_window(workspace, "Windows Firewall", &|class, title| {
            class == "MMCMainFrame" && title.contains("Firewall")
        })?;
        // Selecting Inbound Rules puts New Rule... in the Actions pane. The
        // searches stay out of the rule list, which UI Automation walks slowly.
        let tree = find(workspace, "the console tree", &|| {
            child(frame, "SysTreeView32", &|_| true)
        })?;
        automation
            .wait(
                workspace,
                "the Inbound Rules node",
                tree,
                UIA_TreeItemControlTypeId,
                &|item| item.name == "Inbound Rules",
            )?
            .click(workspace)?;
        // New Rule... does nothing until the snap-in has listed the rules.
        find(workspace, "the inbound rules", &|| {
            child(frame, "SysListView32", &|list| unsafe {
                SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 > 0
            })
        })?;
        settle(workspace, 1000);
        let actions = find(workspace, "the Actions pane", &|| {
            child(frame, "NativeHWNDHost", &|host| {
                window_text(host) == "ActionsPaneView"
            })
        })?;
        let new_rule = automation.wait(
            workspace,
            "New Rule... in the Actions pane",
            actions,
            UIA_ButtonControlTypeId,
            &|button| button.name == "New Rule...",
        )?;
        proof("background-firewall-inbound-rules.bmp")?;
        let before = background::windows()?;
        new_rule.click(workspace)?;
        let wizard = wait_for_job_window(workspace, "the New Inbound Rule Wizard", &|_, title| {
            title == "New Inbound Rule Wizard"
        })?;
        // The wizard's WinForms radio buttons and buttons take clicks.
        let port = automation.wait(
            workspace,
            "the Port rule type",
            wizard,
            UIA_RadioButtonControlTypeId,
            &|radio| radio.name == "Port",
        )?;
        port.click(workspace)?;
        wait_until(workspace, "a click to select Port", 5, &|_| port.selected())?;
        automation
            .wait(
                workspace,
                "the wizard's Next button",
                wizard,
                UIA_ButtonControlTypeId,
                &|button| button.name.starts_with("Next"),
            )?
            .click(workspace)?;
        automation.wait(
            workspace,
            "the Protocol and Ports step",
            wizard,
            UIA_RadioButtonControlTypeId,
            &|radio| radio.name == "TCP",
        )?;
        proof("background-firewall-wizard.bmp")?;

        // Cancel closes the wizard. A posted click on it once crashed the
        // snap-in: the Cancel button's mouse-up hit ObjectDisposedException.
        automation
            .wait(
                workspace,
                "the wizard's Cancel button",
                wizard,
                UIA_ButtonControlTypeId,
                &|button| button.name == "Cancel",
            )?
            .click(workspace)?;
        wait_until(workspace, "Cancel to close the wizard", 10, &|_| {
            background::windows().is_ok_and(|windows| !windows.contains(&wizard))
        })?;
        settle(workspace, 2000);
        proof("background-firewall-cancelled.bmp")?;
        let opened = background::windows()?
            .into_iter()
            .filter(|window| !before.contains(window))
            .map(window_text)
            .collect::<Vec<_>>();
        anyhow::ensure!(opened.is_empty(), "cancelling the wizard opened {opened:?}");
        anyhow::ensure!(
            unsafe { IsWindowEnabled(frame).as_bool() },
            "the Firewall window stayed disabled after the wizard closed"
        );
        println!("Session 0 Firewall New Rule wizard clicks and Cancel passed");
        Ok(())
    })
}

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn resource_monitor_arrows_toggle_sections() -> anyhow::Result<()> {
    in_workspace(|workspace| {
        let automation = Automation::new()?;
        workspace.launch(pin("resmon.exe", ""))?;
        let monitor = wait_for_job_window(workspace, "Resource Monitor", &|class, title| {
            class == "WdcWindow" && title == "Resource Monitor"
        })?;
        let cpu = automation.wait(
            workspace,
            "the CPU section",
            monitor,
            UIA_GroupControlTypeId,
            &|group| group.id == "expandoCpu",
        )?;
        // The CPU section's arrow collapses its table, and expands it again.
        let inside = |inner: RECT, outer: RECT| {
            inner.left >= outer.left
                && inner.right <= outer.right
                && inner.top >= outer.top
                && inner.bottom <= outer.bottom
        };
        let area = cpu.bounds();
        let arrow = automation.wait(
            workspace,
            "the CPU section's arrow",
            monitor,
            UIA_ButtonControlTypeId,
            &|button| button.id == "arrow" && inside(button.rect, area),
        )?;
        let height = || {
            let rect = cpu.bounds();
            rect.bottom - rect.top
        };
        let expanded = height();
        arrow.click(workspace)?;
        wait_until(workspace, "the arrow to collapse the CPU table", 5, &|_| {
            height() < expanded / 2
        })
        .with_context(|| format!("the CPU section is {} px high", height()))?;
        proof("background-resource-monitor-collapsed.bmp")?;
        arrow.click(workspace)?;
        wait_until(workspace, "the arrow to expand the CPU table", 5, &|_| {
            height() == expanded
        })
        .with_context(|| format!("the CPU section is {} px high", height()))?;

        // The chart pane's arrow collapses the charts, which widens the
        // tables, and expands them again.
        let width = || {
            let rect = cpu.bounds();
            rect.right - rect.left
        };
        let narrow = width();
        automation
            .wait(
                workspace,
                "the chart pane's arrow",
                monitor,
                UIA_ButtonControlTypeId,
                &|button| button.name == "Collapse Charts",
            )?
            .click(workspace)?;
        wait_until(workspace, "the arrow to collapse the charts", 5, &|_| {
            width() > narrow + 100
        })
        .with_context(|| format!("the CPU section is {} px wide", width()))?;
        proof("background-resource-monitor-charts-collapsed.bmp")?;
        automation
            .wait(
                workspace,
                "the collapsed chart pane's arrow",
                monitor,
                UIA_ButtonControlTypeId,
                &|button| button.name == "Expand Charts",
            )?
            .click(workspace)?;
        wait_until(workspace, "the arrow to expand the charts", 5, &|_| {
            width() == narrow
        })
        .with_context(|| format!("the CPU section is {} px wide", width()))?;
        println!("Session 0 Resource Monitor table and chart arrows passed");
        Ok(())
    })
}
