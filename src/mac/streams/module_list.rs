use super::*;

impl MiniDumpWriter {
    fn write_module_list(
        &mut self,
        buffer: &mut DumpBuf,
        dumper: &TaskDumper,
    ) -> Result<MDRawDirectory, WriterError> {
        // The list of modules is pretty critical information, but there could
        // still be useful information in the minidump without them if we can't
        // retrieve them for some reason
        let modules = self.read_loaded_modules(dumper).unwrap_or_default();

        let list_header = MemoryWriter::<u32>::alloc_with_val(buffer, modules.len() as u32)?;

        let mut dirent = MDRawDirectory {
            stream_type: MDStreamType::ModuleListStream as u32,
            location: list_header.location(),
        };

        if !modules.is_empty() {
            let mapping_list = MemoryArrayWriter::<MDRawModule>::alloc_from_iter(buffer, modules)?;
            dirent.location.data_size += mapping_list.location().data_size;
        }

        Ok(dirent)
    }

    fn read_loaded_modules(
        &self,
        buf: &mut DumpBuf,
        dumper: &TaskDumper,
    ) -> Result<Vec<MDRawModule>, WriterError> {
        let mut images = dumper.read_images()?;

        // Apparently MacOS will happily list the same image multiple times
        // for some reason, so sort the images by load address and remove all
        // of the duplicates
        images.sort();
        images.dedup();

        let mut modules = Vec::with_capacity(images.len());
        let mut has_main_executable = false;

        for image in images {
            if let Ok((module, is_main_executable)) = self.read_module(image) {
                // We want to keep the modules sorted by their load address except
                // in the case of the main executable image which we want to put
                // first as it is most likely the culprit, or at least generally
                // the most interesting module for human and machine inspectors
                if is_main_executable {
                    modules.insert(0, module);
                    has_main_executable = true;
                } else {
                    modules.push(module)
                };
            }
        }

        if !has_main_executable {
            Err(WriterError::NoExecutableImage)
        } else {
            Ok(images)
        }
    }

    fn read_module(
        &self,
        image: ImageInfo,
        buf: &mut DumpBuf,
        dumper: &TaskDumper,
    ) -> Result<(MDRawModule, bool), WriterError> {
        struct ImageSizes {
            vm_addr: u64,
            vm_size: u64,
            slide: isize,
        }

        let mut sizes = None;
        let mut version = None;
        let mut uuid = None;

        {
            let load_commands = dumper.get_load_commands(&image)?;

            for lc in load_commands.iter() {
                match lc {
                    mach::LoadCommand::Segment(seg) if sizes.is_none() => {
                        if seg.segment_name[..7] == b"__TEXT\0" {
                            let slide = if seg.file_off == 0 && seg.file_size != 0 {
                                image.load_address - seg.vm_addr
                            } else {
                                0
                            };

                            sizes = Some(ImageSizes {
                                vm_addr: seg.vm_addr,
                                vm_size: seg.vm_size,
                                slide,
                            });
                        }
                    }
                    mach::LoadCommand::Dylib(dylib) if version.is_none() => {
                        version = Some(dylib.current_version);
                    }
                    mach::LoadCommand::Uuid(img_id) if uuid.is_none() => {
                        uuid = Some(img_id.uuid);
                    }
                }

                if image_sizes.is_some() && image_version.is_some() && image_uuid.is_some() {
                    break;
                }
            }
        }

        let image_sizes = image_sizes.ok_or_else(|| WriterError::InvalidMachHeader)?;
        let uuid = image_uuid.ok_or_else(|| WriterError::UnknownUuid)?;

        let file_path = if image.file_path != 0 {
            dumper.read_string(image.file_path)?.unwrap_or_default()
        } else {
            String::new()
        };

        let module_name = write_string_to_location(buf, &file_path)?;

        let mut raw_module = MDRawModule {
            base_of_image: image_sizes.vm_addr + image_sizes.slide,
            size_of_image: image_sizes.vm_size as u32,
            module_name_rva: module_name.rva,
            ..Default::default()
        };

        // Version info is not available for the main executable image since
        // it doesn't issue a LC_ID_DYLIB load command
        if let Some(version) = &image_version {
            raw_module.version_info.signature = format::VS_FFI_SIGNATURE;
            raw_module.version_info.struct_version = format::VS_FFI_STRUCVERSION;

            // Convert MAC dylib version format, which is a 32 bit number, to the
            // format used by minidump.  The mac format is <16 bits>.<8 bits>.<8 bits>
            // so it fits nicely into the windows version with some massaging
            // The mapping is:
            //    1) upper 16 bits of MAC version go to lower 16 bits of product HI
            //    2) Next most significant 8 bits go to upper 16 bits of product LO
            //    3) Least significant 8 bits go to lower 16 bits of product LO
            raw_module.version_info.file_version_hi = version >> 16;
            raw_module.version_info.file_version_lo = ((version & 0xff00) << 8) | (version & 0xff);
        }

        let module_name = if let Some(sep_index) = file_path.rfind('/') {
            &file_path[sep_index + 1..]
        } else if file_path.is_empty() {
            "<Unknown>"
        } else {
            &file_path
        };

        #[derive(scroll::Pwrite, scroll::SizeWith)]
        struct CvInfoPdb {
            cv_signature: u32,
            signature: format::GUID,
            age: u32,
        }

        let cv = MemoryWriter::alloc_with_val(
            buf,
            CvInfoPdb {
                cv_signature: format::CvSignature::Pdb70,
                age: 0,
                signature: uuid.into(),
            },
        )?;

        // Note that we don't use write_string_to_location here as the module
        // name is a simple 8-bit string, not 16-bit like most other strings
        // in the minidump, and is directly part of the record itself, not an rva
        buf.write_all(module_name.as_bytes())?;
        buf.write_all(&[0])?; // null terminator

        let mut cv_location = cv.location();
        cv_location.size += module_name.len() as u32 + 1;
        raw_module.cv_record = cv_location;

        Ok((raw_module, image_version.is_none()))
    }
}
