use crate::mac::mach_helpers as mach;
use mach2::mach_types as mt;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum TaskDumpError {
    #[error("kernel error {syscall} {error})")]
    Kernel {
        syscall: &'static str,
        error: mach::KernelError,
    },
    #[error("detected an invalid mach image header")]
    InvalidMachHeader,
}

/// Wraps a mach call in a Result
macro_rules! mach_call {
    ($call:expr) => {{
        // SAFETY: syscall
        let kr = unsafe { $call };
        if kr == mach::KERN_SUCCESS {
            Ok(())
        } else {
            // This is ugly, improvements to the macro welcome!
            let mut syscall = stringify!($call);
            if let Some(i) = sc.find('(') {
                syscall = &syscall[..i];
            }
            Err(TaskDumpError::Kernel {
                syscall,
                error: kr.into(),
            })
        }
    }};
}

// dyld_image_info
#[repr(C)]
pub struct ImageInfo {
    load_address: u64,
    file_path: u64,
    file_mod_date: u64,
}

impl PartialEq for ImageInfo {
    fn eq(&self, o: &Self) -> bool {
        self.load_address == o.load_address
    }
}

impl Eq for ImageInfo {}

impl Ord for ImageInfo {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.load_address.cmp(&o.load_address)
    }
}

impl PartialOrd for ImageInfo {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

/// Describes a region of virtual memory
pub struct VMRegionInfo {
    pub info: mach::vm_region_submap_info_64,
    pub range: std::ops::Range<u64>,
}

/// Similarly to PtraceDumper for Linux, this provides access to information
/// for a task (MacOS process)
pub struct TaskDumper {
    task: mt::task_t,
    page_size: usize,
}

impl TaskDumper {
    /// Constructs a [`TaskDumper`] for the specified task
    pub fn new(task: mt::task_t) -> Self {
        Self {
            task,
            // SAFETY: syscall
            page_size: unsafe { libc::getpagesize() },
        }
    }

    /// Reads a block of memory from the task
    pub fn read_task_memory<T: Sized>(
        &self,
        address: u64,
        count: usize,
    ) -> Result<Vec<T>, TaskDumpError> {
        let length = count * std::mem::size_of::<T>();

        // use the negative of the page size for the mask to find the page address
        let page_address = address & -self.page_size;
        let last_page_address = (address + length + self.page_size - 1) & -self.page_size;

        let page_size = last_page_address - page_address;
        let mut local_start = std::ptr::null_mut();
        let mut local_length = 0;

        mach_call!(mach::mach_vm_read(
            self.task,
            page_address,
            page_size,
            &mut local_start,
            &mut local_length
        ))?;

        let mut buffer = Vec::with_capacity(count);

        let task_buffer =
            std::slice::from_raw_parts(local_start.offset(address - page_address).cast(), count);
        buffer.extend_from_slice(task_buffer);

        // Don't worry about the return here, if something goes wrong there's probably
        // not much we can do about it, and we have what we want anyways
        let _res = mach_call!(mach::mach_vm_deallocate(
            mach::mach_task_self(),
            local_start,
            local_length
        ));

        Ok(buffer)
    }

    /// Reads a null terminated string starting at the specified address. This
    /// is a specialization of [`read_task_memory`] since strings can span VM
    /// regions.
    ///
    /// This string is capped at 8k which should never be close to being hit as
    /// it is only used for file paths for loaded modules, but then again, this
    /// is MacOS, so who knows what insanity goes on.
    ///
    /// # Errors
    ///
    /// Fails if the address cannot be read for some reason, or the string is
    /// not utf-8.
    fn read_string(&self, addr: u64) -> Result<Option<String>, TaskDumpError> {
        // The problem is we don't know how much to read until we know how long
        // the string is. And we don't know how long the string is, until we've read
        // the memory!  So, we'll try to read kMaxStringLength bytes
        // (or as many bytes as we can until we reach the end of the vm region).
        let get_region_size = || {
            let region = self.get_vm_region(addr)?;

            let mut size_to_end = region.range.end - addr;

            // If the remaining is less than 4k, check if the next region is
            // contiguous, and extend the memory that could contain the string
            // to include it
            if size_to_end < 4 * 1024 {
                let maybe_adjacent = self.get_vm_region(region.range.end)?;

                if maybe_adjacent.range.start == region.range.end {
                    size_to_end += maybe_adjacent.range.end - maybe_adjacent.range.start;
                }
            }

            Ok(size_to_end)
        };

        if let Ok(size_to_end) = get_region_size() {
            let mut bytes = self.read_task_memory(addr, size_to_end)?;

            // Find the null terminator and truncate our string
            if let Some(null_pos) = bytes.iter().position(|c| c == 0) {
                bytes.resize(null_pos, 0);
            }

            String::from_utf8(bytes).map(Some)?
        } else {
            Ok(None)
        }
    }

    /// Retrives information on the virtual memory region the specified address
    /// is located within
    pub fn get_vm_region(&self, addr: u64) -> Result<VMRegionInfo, TaskDumpError> {
        let mut region_base = addr;
        let mut region_size = 0;
        let mut nesting_level = 0;
        let mut region_info = 0;
        let mut submap_info = std::mem::MaybeUninit::<mach::vm_region_submap_info_64>::uninit();

        // mach/vm_region.h
        const VM_REGION_SUBMAP_INFO_COUNT_64: u32 =
            (std::mem::size_of::<mach::vm_region_submap_info_64>() / std::mem::size_of::<u32>())
                as u32;

        let mut info_count = VM_REGION_SUBMAP_INFO_COUNT_64;

        mach_call!(mach_vm_region_recurse(
            self.task,
            &mut region_base,
            &mut region_size,
            &mut nesting_level,
            submap_info.as_mut_ptr().cast(),
            &mut info_count,
        ))?;

        Ok(VMRegionInfo {
            // SAFETY: this will be valid if the syscall succeeded
            info: unsafe { submap_info.assume_init() },
            range: region_base..region_base + region_size,
        })
    }

    /// Retrieves the state of the specified thread. The state is is an architecture
    /// specific block of CPU context ie register state.
    pub fn read_thread_state(&self, tid: u32) -> Result<mach::ThreadState, TaskDumpError> {
        let mut thread_state = mach::ThreadState::default();

        mach_call!(mach::thread_get_state(
            tid,
            THREAD_STATE_FLAVOR,
            thread_state.state.as_mut_ptr(),
            &mut thread_state.state_size,
        ))?;

        Ok(thread_state)
    }

    /// Reads the specified task information
    pub fn task_info<T: mach::TaskInfo>(&self) -> Result<T, TaskDumpError> {
        let mut info = std::mem::MaybeUninit::<T>::uninit();
        let mut count = (std::mem::size_of::<T>() / std::mem::size_of::<u32>()) as u32;

        mach_call!(mach::task::task_info(
            self.task,
            T::FLAVOR,
            info.as_mut_ptr().cast(),
            &mut count
        ))?;

        // SAFETY: this will be initialized if the call succeeded
        unsafe { Ok(info.assume_init()) }
    }

    /// Retrieves all of the images loaded in the task. Note that there may be
    /// multiple images with the same load address.
    pub fn read_images(&self) -> Result<Vec<ImageInfo>, TaskDumpError> {
        impl mach::TaskInfo for mach::task_info::task_dyld_info {
            const FLAVOR: mach::task_info::TASK_DYLD_INFO;
        }

        // Retrieve the address at which the list of loaded images is located
        // within the task
        let all_images_addr = {
            let dyld_info = self.task_info::<mach::task_info::task_dyld_info>()?;
            dyld_info.all_image_info_addr
        };

        // dyld_all_image_infos defined in usr/include/mach-o/dyld_images.h, we
        // only need a couple of fields at the beginning
        #[repr(C)]
        struct AllImagesInfo {
            version: u32, // == 1 in Mac OS X 10.4
            info_array_count: u32,
            info_array_addr: u64,
        }

        // Here we make the assumption that dyld loaded at the same address in
        // the crashed process vs. this one.  This is an assumption made in
        // "dyld_debug.c" and is said to be nearly always valid.
        let dyld_all_info_buf =
            self.read_task_memory::<u8>(all_images_addr, std::mem::size_of::<AllImagesInfo>())?;
        // SAFETY: this is fine as long as the kernel isn't lying to us
        let all_dyld_info: &AllImagesInfo = unsafe { &*(dyld_all_info_buf.as_ptr().cast()) };

        self.read_task_memory::<ImageInfo>(
            all_dyld_info.info_array_addr,
            all_dyld_info.info_array_count as usize,
        )
    }

    /// Retrieves the load commands for the specified image
    pub fn read_load_commands(&self, img: &ImageInfo) -> Result<mach::LoadComands, TaskDumpError> {
        let mach_header_buf =
            self.read_task_memory::<u8>(img.load_address, std::mem::size_of::<mach::MachHeader>())?;

        let header: &mach::MachHeader = &*(mach_header_buf.as_ptr().cast());

        if header.magic != mach::MH_MAGIC_64 {
            return Err(TaskDumpError::InvalidMachHeader);
        }

        // Read the load commands which immediately follow the image header from
        // the task memory. Note that load commands vary in size so we need to
        // retrieve the memory as a raw byte buffer that we can then iterate
        // through and step according to the size of each load command
        let load_commands_buf = self.read_task_memory::<u8>(
            image.load_address + std::mem::size_of::<MachHeader>() as u64,
            header.size_commands as usize,
        )?;

        Ok(mach::LoadComands {
            buffer: load_commands_buf,
            count: header.num_commands,
        })
    }
}
