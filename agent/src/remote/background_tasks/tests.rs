use super::*;
fn fixture() -> Box<RefCell<State>> {
    let state = create_window().unwrap();
    state.borrow_mut().snapshot = Snapshot {
        memory_total: 8 * 1024 * 1024 * 1024,
        processes: (0..100)
            .map(|i| Process {
                pid: 1000 + i,
                created: Some(i as u64 + 1),
                name: format!("Process {i:03}.exe"),
                description: format!("Process {i:03}"),
                memory: Some(i as usize * 1024 * 1024),
                cpu: Some(i as f64 / 10.0),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    state.borrow_mut().rebuild();
    state
}
fn cleanup(state: Box<RefCell<State>>) {
    let state = state.into_inner();
    unsafe {
        SetWindowLongPtrW(state.hwnd, GWLP_USERDATA, 0);
        DestroyWindow(state.hwnd).unwrap();
        let _ = ImageList_Destroy(Some(state.images));
        let _ = DeleteObject(state.font.into());
        let _ = DeleteObject(state.heading_font.into());
    }
}
#[test]
fn padded_icons_keep_color_and_transparent_row_padding() {
    unsafe {
        let images = ImageList_Create(16, 28, ILC_COLOR24 | ILC_MASK, 1, 1);
        let source = LoadIconW(None, IDI_APPLICATION).unwrap();
        assert_eq!(padded_icon(images, source), 0);
        let icon = ImageList_GetIcon(images, 0, ILD_TRANSPARENT);
        let mut info = ICONINFO::default();
        GetIconInfo(icon, &mut info).unwrap();
        let dc = CreateCompatibleDC(None);
        let mut bitmap = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 16,
                biHeight: -28,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; 16 * 28 * 4];
        let count = GetDIBits(
            dc,
            info.hbmColor,
            0,
            28,
            Some(pixels.as_mut_ptr().cast()),
            &mut bitmap,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        let _ = DeleteObject(info.hbmColor.into());
        let _ = DeleteObject(info.hbmMask.into());
        let _ = DestroyIcon(icon);
        let _ = ImageList_Destroy(Some(images));
        assert_eq!(count, 28);
        assert!(
            pixels[6 * 16 * 4..22 * 16 * 4]
                .chunks_exact(4)
                .any(|p| p[0] != 0 || p[1] != 0 || p[2] != 0),
            "Icon became a solid black block"
        );
        assert!(
            pixels[..6 * 16 * 4]
                .chunks_exact(4)
                .all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0),
            "Row padding must stay transparent"
        );
    }
}

#[test]
fn refresh_preserves_selected_identity_and_scroll_anchor() {
    let state = fixture();
    {
        let mut s = state.borrow_mut();
        s.tab = Tab::Details;
        s.configure();
        s.select(70);
        let selected = s.selected().unwrap().key.clone();
        let top = unsafe { SendMessageW(s.list, LVM_GETTOPINDEX, None, None).0 };
        assert!(top > 0);
        let anchor = s.rows[top as usize].key.clone();
        s.snapshot.processes.push(Process {
            pid: 999,
            created: Some(1),
            name: "A newly started process".into(),
            ..Default::default()
        });
        s.rebuild();
        assert_eq!(s.selected().unwrap().key, selected);
        let top = unsafe { SendMessageW(s.list, LVM_GETTOPINDEX, None, None).0 };
        assert_eq!(s.rows[top as usize].key, anchor);
        let p = s
            .snapshot
            .processes
            .iter_mut()
            .find(|p| process_key(p) == selected)
            .unwrap();
        p.created = Some(9000);
        s.rebuild();
        assert!(
            s.selected().is_none(),
            "A reused PID must not inherit selection"
        );
    }
    cleanup(state);
}
#[test]
fn numeric_sort_and_expand_groups() {
    let state = fixture();
    {
        let mut s = state.borrow_mut();
        s.grouped = false;
        s.sort = 3;
        s.descending = true;
        let rows = s.make_rows();
        assert_eq!(rows[0].cells[3], "99.0 MB");
        s.snapshot.processes[0].description = "Same app".into();
        s.snapshot.processes[1].description = "Same app".into();
        s.rebuild();
        let index = s
            .rows
            .iter()
            .position(|r| r.cells[0] == "Same app (2)")
            .unwrap();
        s.select(index);
        s.expand();
        assert_eq!(s.rows.iter().filter(|r| r.indent == 1).count(), 2);
        let group = s
            .rows
            .iter()
            .find(|r| r.cells[0] == "Same app (2)")
            .unwrap();
        assert_eq!(group.cells[3], "1.0 MB");
        s.expand();
        assert_eq!(s.rows.iter().filter(|r| r.indent == 1).count(), 0);
    }
    cleanup(state);
}
#[test]
fn tabs_compact_mode_and_paused_refresh() {
    let state = fixture();
    {
        let mut s = state.borrow_mut();
        for index in 0..7 {
            s.tab = Tab::from_index(index);
            s.configure();
            assert_eq!(s.tab.index(), index);
            let style = unsafe { GetWindowLongW(s.list, GWL_STYLE) as u32 };
            assert_eq!(style & WS_VISIBLE.0 != 0, s.tab != Tab::Performance);
            if s.tab == Tab::Details {
                assert_ne!(style & WS_HSCROLL.0, 0, "Wide columns must be reachable");
            }
        }
        s.command(COMPACT).unwrap();
        assert!(s.compact);
        assert_eq!(s.tab, Tab::Processes);
        s.command(COMPACT).unwrap();
        assert!(!s.compact);
        s.command(SPEED_PAUSED).unwrap();
        assert_eq!(s.interval, 0);
        s.command(SPEED_NORMAL).unwrap();
        assert_eq!(s.interval, 1000);
    }
    cleanup(state);
}
