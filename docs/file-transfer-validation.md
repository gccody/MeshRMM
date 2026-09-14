# Native file transfer validation

Validated September 13–14, 2026 using the installed macOS viewer and the Windows
agent at `192.168.1.152`, through a dashboard-authorized remote session.

| Method | Live result |
| --- | --- |
| Send | Native macOS picker transferred a nested folder, including an empty folder, into the Windows user's Documents transfer folder. |
| Receive | Native Windows picker transferred mixed files and folders into the macOS user's Documents transfer folder. |
| Explorer drop | A Finder file became a native Windows drop in the open Explorer destination; destination SHA-256 matched. |
| Desktop drop | A file appeared on the Windows desktop; destination SHA-256 matched. |
| Browser drop | A local browser drop page received the native `File`, read its contents, and displayed the matching SHA-256. |
| Declined drop | A declined native target saved the verified file into Documents instead. |
| Client-to-agent clipboard | Finder Copy followed by remote Ctrl+V pasted files and a nested folder onto the Windows desktop. |
| Agent-to-client clipboard | Explorer Copy followed by Finder Paste delivered a Windows-origin file and nested folder, including an empty directory. |
| Larger-file round trip | An 8 MiB binary sent through Send and returned through Receive matched SHA-256 on both endpoints. |

The round-trip binary's SHA-256 was
`7d212b9c884f5c77896de960ae17cc341cda43b14d6a971f34ca29ebd4badf7f`.

Windows workspace formatting, Clippy with warnings denied, and workspace tests
passed. macOS protocol, file-transfer, and viewer tests passed, as did Clippy for
the file-transfer crate and viewer. Existing hardware-dependent capture tests
remain ignored by the standard test suite. The Windows viewer was built and
unit-tested; the interactive client used for these end-to-end tests was macOS.

The native Windows drop source owns an OLE message queue. It seeds mouse messages
on that queue, supplies the requested screen position through a thread-scoped
message hook, and pumps drag-over events before button release so browser targets
can negotiate their drop effect. The file helper runs as the interactive user.

## Receiver progress windows

The September 14 progress update was installed on both endpoints. A 16 MiB
Send/Receive round trip showed the standard Windows shell progress dialog on the
Agent during Send (observed at 33%) and the AppKit progress window on the Mac
during Receive (observed at 87–88%). Both showed the filename, overall percentage,
transferred size, and item count, and both closed automatically on completion.
The file matched SHA-256
`341aacac661ccb210720bedaa9ead5d668fe5ea41a73532fc147c71e34040df1`
on both computers. Windows workspace Clippy/tests and macOS protocol/viewer/
file-transfer checks passed. Progress totals are included in the existing
nested-folder wire round-trip test.
