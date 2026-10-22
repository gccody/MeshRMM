// Files the user chose to upload, from a file picker, a folder picker or a
// drop, each with its path under the folder it goes into.

export type PickedFile = {
  file: File;
  // The path below the destination, `/`-separated, ending in the file's
  // name: `report.pdf`, or `Photos/2026/beach.jpg` from a folder.
  relative: string;
};

export type Picked = {
  files: PickedFile[];
  // Folders to create even if no file goes in them, `/`-separated.
  folders: string[];
};

// What a file or folder picker chose. A folder picker names each file with
// its path from the chosen folder.
export function fromFileList(list: FileList | null): Picked {
  const files = Array.from(list ?? [], (file) => ({ file, relative: file.webkitRelativePath || file.name }));
  return { files, folders: [] };
}

function readEntries(reader: FileSystemDirectoryReader): Promise<FileSystemEntry[]> {
  return new Promise((resolve, reject) => reader.readEntries(resolve, reject));
}

function fileOf(entry: FileSystemFileEntry): Promise<File> {
  return new Promise((resolve, reject) => entry.file(resolve, reject));
}

// Everything dropped: files, and folders with all they hold. A drop that
// isn't files (text, a link) has none.
export async function fromDrop(transfer: DataTransfer): Promise<Picked> {
  const picked: Picked = { files: [], folders: [] };
  const entries = Array.from(transfer.items)
    .filter((item) => item.kind === "file")
    .map((item) => item.webkitGetAsEntry())
    .filter((entry): entry is FileSystemEntry => entry !== null);
  if (!entries.length) return fromFileList(transfer.files);
  const pending: { entry: FileSystemEntry; relative: string }[] = entries.map((entry) => ({ entry, relative: entry.name }));
  while (pending.length) {
    const { entry, relative } = pending.pop()!;
    if (entry.isFile) {
      picked.files.push({ file: await fileOf(entry as FileSystemFileEntry), relative });
    } else if (entry.isDirectory) {
      picked.folders.push(relative);
      const reader = (entry as FileSystemDirectoryEntry).createReader();
      // A reader gives a folder's entries a batch at a time.
      for (let batch = await readEntries(reader); batch.length; batch = await readEntries(reader)) {
        for (const child of batch) pending.push({ entry: child, relative: `${relative}/${child.name}` });
      }
    }
  }
  return picked;
}

// Whether a drag carries files, as opposed to text or the page's own rows.
export const draggingFiles = (transfer: DataTransfer | null) => Boolean(transfer?.types.includes("Files"));

// The folders that need creating because no file will create them: those
// with nothing at all inside.
export function emptyFolders(picked: Picked) {
  const used = new Set<string>();
  for (const { relative } of picked.files) {
    const parts = relative.split("/");
    for (let depth = 1; depth < parts.length; depth++) used.add(parts.slice(0, depth).join("/"));
  }
  return picked.folders.filter((folder) => !used.has(folder) && !picked.folders.some((other) => other.startsWith(`${folder}/`)));
}
