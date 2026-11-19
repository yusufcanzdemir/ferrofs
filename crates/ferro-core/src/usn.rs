//! USN journal: full enumeration (SPEC §2.2 phase A) and live updates (§2.4).

use std::ffi::c_void;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_HANDLE_EOF, ERROR_IO_PENDING, ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE,
    ERROR_OPERATION_ABORTED, WAIT_TIMEOUT,
};
use windows::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
};
use windows::Win32::System::Ioctl::{
    CREATE_USN_JOURNAL_DATA, FSCTL_CREATE_USN_JOURNAL, FSCTL_ENUM_USN_DATA,
    FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0,
    NTFS_VOLUME_DATA_BUFFER, READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0, USN_REASON_FILE_DELETE,
    USN_REASON_RENAME_OLD_NAME,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE};
use windows::core::{Owned, PCWSTR};

use crate::error::{Error, Result, Win32Context, win32_code};
use crate::fold::FoldOptions;
use crate::index::{Index, Name, NewEntry, record_number};
use crate::volume::Volume;

/// Output buffer size for enumeration and journal reads.
const BUFFER_BYTES: usize = 1 << 20;

/// Position of a volume's change journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalInfo {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
}

/// One USN record. `name` borrows a buffer that is reused for the next record.
#[derive(Debug, Clone, Copy)]
pub struct UsnRecord<'a> {
    pub frn: u64,
    pub parent_frn: u64,
    pub usn: i64,
    /// `USN_REASON_*` bits.
    pub reason: u32,
    /// Win32 `FILE_ATTRIBUTE_*` bits.
    pub attributes: u32,
    pub name: &'a [u16],
}

impl UsnRecord<'_> {
    pub fn to_entry(&self) -> NewEntry<'_> {
        NewEntry {
            frn: self.frn,
            parent_frn: self.parent_frn,
            name: Name::Utf16(self.name),
            attributes: self.attributes,
            size: 0,
            alloc_size: 0,
            mtime: 0,
        }
    }
}

/// Queries the journal, creating it if the volume has none.
pub fn query_journal(volume: &Volume) -> Result<JournalInfo> {
    let mut data = USN_JOURNAL_DATA_V0::default();
    match ioctl(volume, FSCTL_QUERY_USN_JOURNAL, &(), &mut data) {
        Ok(_) => {}
        Err(err) if err.win32_code() == Some(ERROR_JOURNAL_NOT_ACTIVE.0) => {
            tracing::info!(root = volume.root(), "creating USN journal");
            let create = CREATE_USN_JOURNAL_DATA {
                MaximumSize: 32 << 20,
                AllocationDelta: 8 << 20,
            };
            ioctl(volume, FSCTL_CREATE_USN_JOURNAL, &create, &mut ())?;
            ioctl(volume, FSCTL_QUERY_USN_JOURNAL, &(), &mut data)?;
        }
        Err(err) => return Err(err),
    }
    Ok(JournalInfo {
        journal_id: data.UsnJournalID,
        first_usn: data.FirstUsn,
        next_usn: data.NextUsn,
    })
}

/// Number of MFT records in use or reserved; a good capacity hint for the index.
pub fn mft_record_count(volume: &Volume) -> Result<u64> {
    let mut data = NTFS_VOLUME_DATA_BUFFER::default();
    ioctl(volume, FSCTL_GET_NTFS_VOLUME_DATA, &(), &mut data)?;
    let length = u64::try_from(data.MftValidDataLength).unwrap_or(0);
    Ok(length / u64::from(data.BytesPerFileRecordSegment.max(1)))
}

/// Calls `f` with raw record bytes (consecutive `USN_RECORD`s) for MFT records numbered
/// `start..end`. Enumeration returns records in record-number order.
fn enumerate_range(
    volume: &Volume,
    journal: &JournalInfo,
    start: u64,
    end: u64,
    mut f: impl FnMut(&[u8]),
) -> Result<()> {
    let mut buffer = AlignedBuffer::new();
    let mut request = MFT_ENUM_DATA_V0 {
        StartFileReferenceNumber: start,
        LowUsn: 0,
        HighUsn: journal.next_usn,
    };
    loop {
        let returned = match ioctl_buffer(volume, FSCTL_ENUM_USN_DATA, &request, &mut buffer, None)
        {
            Ok(n) => n.unwrap_or(0),
            Err(err) if err.win32_code() == Some(ERROR_HANDLE_EOF.0) => return Ok(()),
            Err(err) => return Err(err),
        };
        let bytes = &buffer.bytes()[..returned];
        let Some(next) = read_u64(bytes, 0) else {
            return Ok(());
        };
        let records = &bytes[8..];
        let cut = range_cut(records, end);
        f(&records[..cut]);
        if cut < records.len() || record_number(next) >= end {
            return Ok(());
        }
        request.StartFileReferenceNumber = next;
    }
}

/// Length of the prefix of `records` whose record numbers are below `end`.
fn range_cut(records: &[u8], end: u64) -> usize {
    let mut at = 0;
    while let Some(length) = read_u32(records, at).and_then(|l| usize::try_from(l).ok()) {
        if length < 16 || at + length > records.len() {
            break;
        }
        match read_u64(records, at + 8) {
            Some(frn) if record_number(frn) >= end => break,
            _ => at += length,
        }
    }
    at
}

/// Calls `f` for every file and folder on the volume (names and hierarchy only).
pub fn enumerate(
    volume: &Volume,
    journal: &JournalInfo,
    mut f: impl FnMut(&UsnRecord<'_>),
) -> Result<usize> {
    let mut name = Vec::with_capacity(256);
    let mut count = 0;
    enumerate_range(volume, journal, 0, u64::MAX, |bytes| {
        count += parse_records(bytes, &mut name, &mut f);
    })?;
    Ok(count)
}

/// Builds the index of a volume from scratch using all CPUs. Returns the index and the
/// journal position from which live updates continue.
pub fn scan(root: &str, fold_options: FoldOptions) -> Result<(Index, JournalInfo)> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    scan_with_threads(root, fold_options, threads)
}

/// [`scan`] with an explicit number of enumeration threads. The MFT is split into equal
/// record-number ranges, each enumerated through its own volume handle; one thread merges
/// the raw record chunks into the index.
pub fn scan_with_threads(
    root: &str,
    fold_options: FoldOptions,
    threads: usize,
) -> Result<(Index, JournalInfo)> {
    let started = Instant::now();
    let volume = Volume::open(root)?;
    let journal = query_journal(&volume)?;
    let records = mft_record_count(&volume).unwrap_or(0);
    let mut index = Index::with_capacity(root, fold_options, usize::try_from(records).unwrap_or(0));

    let threads = u64::try_from(threads.max(1)).unwrap_or(1);
    let per_thread = records.div_ceil(threads).max(1);
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(16);
    let mut count = 0;
    std::thread::scope(|scope| -> Result<()> {
        let workers: Vec<_> = (0..threads)
            .map(|i| {
                let tx = tx.clone();
                let start = i * per_thread;
                // The last range is open-ended in case the MFT grew since we asked.
                let end = if i + 1 == threads {
                    u64::MAX
                } else {
                    start + per_thread
                };
                scope.spawn(move || -> Result<()> {
                    let volume = Volume::open(root)?;
                    enumerate_range(&volume, &journal, start, end, |bytes| {
                        // Receiver gone means the merge failed; stop quietly.
                        let _ = tx.send(bytes.to_vec());
                    })
                })
            })
            .collect();
        drop(tx);

        let mut name = Vec::with_capacity(256);
        for chunk in rx {
            count += parse_records(&chunk, &mut name, &mut |record: &UsnRecord<'_>| {
                index.upsert(&record.to_entry());
            });
        }
        for worker in workers {
            worker.join().unwrap_or(Ok(()))?;
        }
        Ok(())
    })?;

    tracing::info!(
        root,
        threads,
        records = count,
        entries = index.len(),
        ms = started.elapsed().as_millis(),
        "volume scanned"
    );
    Ok((index, journal))
}

/// Reads new journal records after a scan.
#[derive(Debug)]
pub struct JournalReader {
    volume: Volume,
    journal_id: u64,
    next_usn: i64,
    buffer: AlignedBuffer,
    name: Vec<u16>,
}

impl JournalReader {
    /// Opens the volume for asynchronous reads and continues from `journal.next_usn`
    /// (normally the value returned by [`scan`]).
    pub fn open(root: &str, journal: JournalInfo) -> Result<Self> {
        Ok(Self {
            volume: Volume::open_overlapped(root)?,
            journal_id: journal.journal_id,
            next_usn: journal.next_usn,
            buffer: AlignedBuffer::new(),
            name: Vec::with_capacity(256),
        })
    }

    /// Position the next read starts from (persisted with snapshots).
    pub fn next_usn(&self) -> i64 {
        self.next_usn
    }

    /// Waits up to `timeout` for changes, then calls `f` for each new record.
    /// Returns the number of records; 0 means the wait timed out.
    pub fn read(&mut self, timeout: Duration, mut f: impl FnMut(&UsnRecord<'_>)) -> Result<usize> {
        // BytesToWaitFor = 1 blocks until data arrives; the timeout is enforced by the
        // overlapped wait (the request's own Timeout field is not honoured reliably).
        let request = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: self.next_usn,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 1,
            UsnJournalID: self.journal_id,
        };
        let returned = match ioctl_buffer(
            &self.volume,
            FSCTL_READ_USN_JOURNAL,
            &request,
            &mut self.buffer,
            Some(timeout),
        ) {
            Ok(Some(n)) => n,
            Ok(None) => return Ok(0),
            Err(err) if err.win32_code() == Some(ERROR_JOURNAL_ENTRY_DELETED.0) => {
                return Err(Error::JournalReset);
            }
            Err(err) => return Err(err),
        };
        let bytes = &self.buffer.bytes()[..returned];
        let Some(next) = read_u64(bytes, 0) else {
            return Ok(0);
        };
        let count = parse_records(&bytes[8..], &mut self.name, &mut f);
        // USNs are non-negative; the first 8 bytes are the next USN to read.
        self.next_usn = i64::try_from(next).unwrap_or(self.next_usn);
        Ok(count)
    }
}

/// Applies one journal record to the index. Returns true if the index changed.
pub fn apply(index: &mut Index, record: &UsnRecord<'_>) -> bool {
    if record.reason & USN_REASON_FILE_DELETE != 0 {
        return index.remove(record.frn);
    }
    if record.reason & USN_REASON_RENAME_OLD_NAME != 0 {
        return false; // the matching RENAME_NEW_NAME record carries the new name
    }
    index.upsert(&record.to_entry()).is_some()
}

/// Parses consecutive `USN_RECORD_V2`/`V3` records. Returns how many were delivered.
fn parse_records(
    mut bytes: &[u8],
    name: &mut Vec<u16>,
    f: &mut impl FnMut(&UsnRecord<'_>),
) -> usize {
    let mut count = 0;
    while let Some(length) = read_u32(bytes, 0).and_then(|l| usize::try_from(l).ok()) {
        if length < 8 || length > bytes.len() {
            break;
        }
        let record = &bytes[..length];
        if let Some(parsed) = parse_one(record, name) {
            f(&parsed);
            count += 1;
        }
        bytes = &bytes[length..];
    }
    count
}

fn parse_one<'n>(record: &[u8], name: &'n mut Vec<u16>) -> Option<UsnRecord<'n>> {
    let major = read_u16(record, 4)?;
    // Field offsets for V2 (64-bit references) and V3 (128-bit references).
    let (frn, parent, usn, reason, attributes, name_len, name_off) = match major {
        2 => (
            read_u64(record, 8)?,
            read_u64(record, 16)?,
            read_u64(record, 24)?,
            read_u32(record, 40)?,
            read_u32(record, 52)?,
            read_u16(record, 56)?,
            read_u16(record, 58)?,
        ),
        // NTFS references fit in the low 64 bits of a 128-bit FILE_ID_128.
        3 => (
            read_u64(record, 8)?,
            read_u64(record, 24)?,
            read_u64(record, 40)?,
            read_u32(record, 56)?,
            read_u32(record, 68)?,
            read_u16(record, 72)?,
            read_u16(record, 74)?,
        ),
        _ => return None,
    };
    let start = usize::from(name_off);
    let raw = record.get(start..start + usize::from(name_len))?;
    name.clear();
    name.extend(
        raw.chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]])),
    );
    Some(UsnRecord {
        frn,
        parent_frn: parent,
        usn: i64::try_from(usn).ok()?,
        reason,
        attributes,
        name,
    })
}

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

/// 8-byte aligned output buffer for FSCTLs that return records.
#[derive(Debug)]
struct AlignedBuffer(Vec<u64>);

impl AlignedBuffer {
    fn new() -> Self {
        Self(vec![0; BUFFER_BYTES / 8])
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: a u64 slice reinterpreted as bytes: same allocation, length * 8 bytes,
        // u8 has no alignment requirement and every bit pattern is valid.
        unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast::<u8>(), self.0.len() * 8) }
    }
}

/// FSCTL with a fixed-size input and output struct.
fn ioctl<I, O>(volume: &Volume, code: u32, input: &I, output: &mut O) -> Result<u32> {
    let in_size = u32::try_from(size_of::<I>()).unwrap_or(0);
    let out_size = u32::try_from(size_of::<O>()).unwrap_or(0);
    // SAFETY: the pointers reference live values of the stated sizes; `device_io` does not
    // return before the request has completed.
    let returned = unsafe {
        device_io(
            volume,
            code,
            (in_size > 0).then_some(std::ptr::from_ref(input).cast::<c_void>()),
            in_size,
            (out_size > 0).then_some(std::ptr::from_mut(output).cast::<c_void>()),
            out_size,
            None,
        )
    }?;
    Ok(returned.unwrap_or(0))
}

/// FSCTL that fills `buffer`. Returns the number of bytes written, or `None` if `timeout`
/// expired first (only possible on overlapped volumes).
fn ioctl_buffer<I>(
    volume: &Volume,
    code: u32,
    input: &I,
    buffer: &mut AlignedBuffer,
    timeout: Option<Duration>,
) -> Result<Option<usize>> {
    let in_size = u32::try_from(size_of::<I>()).unwrap_or(0);
    let out_size = u32::try_from(buffer.0.len() * 8).unwrap_or(0);
    // SAFETY: `input` and the buffer are live for the whole call and `device_io` does not
    // return before the request has completed or been cancelled.
    let returned = unsafe {
        device_io(
            volume,
            code,
            Some(std::ptr::from_ref(input).cast::<c_void>()),
            in_size,
            Some(buffer.0.as_mut_ptr().cast::<c_void>()),
            out_size,
            timeout,
        )
    }?;
    Ok(returned.map(|n| n as usize))
}

/// `DeviceIoControl` for both synchronous and overlapped volume handles.
///
/// # Safety
/// `input`/`output` must be valid for `in_size`/`out_size` bytes until this returns.
/// On overlapped handles this function always waits for the request to finish (or for its
/// cancellation to complete) before returning, so the buffers are never used afterwards.
unsafe fn device_io(
    volume: &Volume,
    code: u32,
    input: Option<*const c_void>,
    in_size: u32,
    output: Option<*mut c_void>,
    out_size: u32,
    timeout: Option<Duration>,
) -> Result<Option<u32>> {
    let handle = volume.handle();
    let mut returned = 0u32;
    if !volume.is_overlapped() {
        // SAFETY: buffers are valid per the function contract; the call is synchronous.
        unsafe {
            DeviceIoControl(
                handle,
                code,
                input,
                in_size,
                output,
                out_size,
                Some(&mut returned),
                None,
            )
        }
        .win32("DeviceIoControl failed")?;
        return Ok(Some(returned));
    }

    // SAFETY: unnamed manual-reset event with default security; owned and closed by Owned.
    let event =
        unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.win32("CreateEventW failed")?;
    // SAFETY: the event was just created and is owned only here.
    let event = unsafe { Owned::new(event) };
    let overlapped = OVERLAPPED {
        hEvent: *event,
        ..Default::default()
    };
    // SAFETY: buffers are valid per the function contract and `overlapped` outlives the
    // request because every path below waits for completion.
    let started = unsafe {
        DeviceIoControl(
            handle,
            code,
            input,
            in_size,
            output,
            out_size,
            None,
            Some(std::ptr::from_ref(&overlapped).cast_mut()),
        )
    };
    if let Err(err) = started
        && win32_code(&err) != Some(ERROR_IO_PENDING.0)
    {
        return Err(err).win32("DeviceIoControl failed");
    }

    let wait_ms = timeout.map_or(INFINITE, |t| {
        u32::try_from(t.as_millis()).unwrap_or(INFINITE - 1)
    });
    // SAFETY: `overlapped` belongs to the request started above.
    match unsafe { GetOverlappedResultEx(handle, &overlapped, &mut returned, wait_ms, false) } {
        Ok(()) => Ok(Some(returned)),
        Err(err) if win32_code(&err) == Some(WAIT_TIMEOUT.0) => {
            // SAFETY: cancels only this request; then wait until it has really finished
            // so the kernel no longer touches `overlapped` or the buffers.
            unsafe {
                let _ = CancelIoEx(handle, Some(&overlapped));
                match GetOverlappedResult(handle, &overlapped, &mut returned, true) {
                    // Completed just before the cancel: keep the data.
                    Ok(()) => Ok(Some(returned)),
                    Err(err) if win32_code(&err) == Some(ERROR_OPERATION_ABORTED.0) => Ok(None),
                    Err(err) => Err(err).win32("cancelled request failed"),
                }
            }
        }
        Err(err) => Err(err).win32("DeviceIoControl failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a USN_RECORD_V2 the way NTFS lays it out.
    fn v2(frn: u64, parent: u64, reason: u32, attributes: u32, name: &str) -> Vec<u8> {
        let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let len = (60 + name.len()).next_multiple_of(8);
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&u32::try_from(len).unwrap().to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&frn.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[24..32].copy_from_slice(&77u64.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[52..56].copy_from_slice(&attributes.to_le_bytes());
        r[56..58].copy_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        r[60..60 + name.len()].copy_from_slice(&name);
        r
    }

    fn collect(bytes: &[u8]) -> Vec<(u64, u64, u32, String)> {
        let mut out = Vec::new();
        let mut name = Vec::new();
        parse_records(bytes, &mut name, &mut |r: &UsnRecord<'_>| {
            out.push((
                r.frn,
                r.parent_frn,
                r.reason,
                String::from_utf16_lossy(r.name),
            ));
        });
        out
    }

    #[test]
    fn parses_v2_records_and_stops_on_truncation() {
        let mut bytes = v2(30, 5, 0, 0x20, "Şehir.txt");
        bytes.extend(v2(31, 30, USN_REASON_FILE_DELETE, 0x10, "dir"));
        let full = collect(&bytes);
        assert_eq!(
            full,
            [
                (30, 5, 0, "Şehir.txt".to_owned()),
                (31, 30, USN_REASON_FILE_DELETE, "dir".to_owned())
            ]
        );
        // A truncated trailing record is ignored, not misread.
        assert_eq!(collect(&bytes[..bytes.len() - 4]).len(), 1);
        assert!(collect(&[]).is_empty());
    }

    #[test]
    fn range_cut_stops_at_end_record() {
        let seq = 7u64 << 48; // sequence numbers must not affect the range check
        let mut bytes = v2(seq | 10, 5, 0, 0, "a");
        let first = bytes.len();
        bytes.extend(v2(seq | 11, 5, 0, 0, "b"));
        let second = bytes.len();
        bytes.extend(v2(seq | 20, 5, 0, 0, "c"));
        assert_eq!(range_cut(&bytes, 11), first);
        assert_eq!(range_cut(&bytes, 12), second);
        assert_eq!(range_cut(&bytes, u64::MAX), bytes.len());
        assert_eq!(range_cut(&bytes, 0), 0);
    }

    #[test]
    fn apply_handles_create_rename_delete() {
        let mut index = Index::new("T:\\", FoldOptions::default());
        let mut name = Vec::new();
        let mut run = |bytes: Vec<u8>, index: &mut Index| {
            parse_records(&bytes, &mut name, &mut |r: &UsnRecord<'_>| {
                apply(index, r);
            });
        };
        run(v2(40, 5, 0x100, 0x10, "docs"), &mut index); // FILE_CREATE
        run(v2(41, 40, 0x100, 0x20, "a.txt"), &mut index);
        assert_eq!(index.path(index.lookup(41).unwrap()), r"T:\docs\a.txt");

        run(
            v2(41, 40, USN_REASON_RENAME_OLD_NAME, 0x20, "a.txt"),
            &mut index,
        );
        run(v2(41, 5, 0x2000, 0x20, "b.txt"), &mut index); // RENAME_NEW_NAME
        assert_eq!(index.path(index.lookup(41).unwrap()), r"T:\b.txt");

        run(
            v2(41, 5, USN_REASON_FILE_DELETE | 0x8000_0000, 0x20, "b.txt"),
            &mut index,
        );
        assert!(index.lookup(41).is_none());
        assert_eq!(index.len(), 1);
    }
}
