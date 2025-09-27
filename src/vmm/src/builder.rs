// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Enables pre-boot setup, instantiation and booting of a Firecracker VMM.

use std::fmt::Debug;
use std::io;
#[cfg(target_arch = "x86_64")]
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::Path;
#[cfg(feature = "gdb")]
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use event_manager::{MutEventSubscriber, SubscriberOps};
use kvm_bindings::{kvm_create_device, kvm_device_type_KVM_DEV_TYPE_VFIO};
use kvm_ioctls::{DeviceFd, IoEventAddress, NoDatamatch, VmFd};
use libc::EFD_NONBLOCK;
use linux_loader::cmdline::Cmdline as LoaderKernelCmdline;
use pci::{
    PciBarConfiguration, PciBarRegionType, PciBdf, PciConfigIo, VfioPciDevice, VfioPciError,
};
use userfaultfd::Uffd;
use utils::time::TimestampUs;
use vfio_ioctls::{VfioContainer, VfioDevice, VfioDeviceFd};
use vm_device::interrupt::{InterruptManager, MsiIrqGroupConfig};
use vm_memory::Address;
#[cfg(target_arch = "aarch64")]
use vm_superio::Rtc;
use vm_superio::Serial;
use vm_system_allocator::{AddressAllocator, GsiApic, SystemAllocator};
use vmm_sys_util::eventfd::EventFd;

use crate::arch::x86_64::layout::MEM_32BIT_RESERVED_START;
use crate::arch::ConfigurationError;
use crate::arch::x86_64::{configure_system_for_boot, load_kernel};
#[cfg(target_arch = "x86_64")]
use crate::arch::{MEM_32BIT_DEVICES_SIZE, MEM_32BIT_DEVICES_START};
#[cfg(target_arch = "aarch64")]
use crate::construct_kvm_mpidrs;
use crate::cpu_config::templates::{
    GetCpuTemplate, GetCpuTemplateError, GuestConfigError, KvmCapability,
};
use crate::device_manager::acpi::ACPIDeviceManager;
#[cfg(target_arch = "x86_64")]
use crate::device_manager::legacy::PortIODeviceManager;
use crate::device_manager::mmio::{MMIODeviceManager, MmioError};
use crate::device_manager::persist::{
    ACPIDeviceManagerConstructorArgs, ACPIDeviceManagerRestoreError, MMIODevManagerConstructorArgs,
};
use crate::device_manager::resources::ResourceAllocator;
use crate::devices::acpi::vmgenid::{VmGenId, VmGenIdError};
#[cfg(target_arch = "aarch64")]
use crate::devices::legacy::RTCDevice;
use crate::devices::legacy::serial::SerialOut;
use crate::devices::legacy::{EventFdTrigger, SerialEventsWrapper, SerialWrapper};
use crate::devices::pci_segment::PciSegment;
use crate::devices::virtio::balloon::Balloon;
use crate::devices::virtio::block::device::Block;
use crate::devices::virtio::device::VirtioDevice;
use crate::devices::virtio::net::Net;
use crate::devices::virtio::rng::Entropy;
use crate::devices::virtio::transport::{MmioTransport, VirtioPciDevice};
use crate::devices::virtio::vsock::{Vsock, VsockUnixBackend};
use crate::devices::{Bus, BusDevice, virtio};
#[cfg(feature = "gdb")]
use crate::gdb;
use crate::initrd::{InitrdConfig, InitrdError};
use crate::interrupt::MsiInterruptManager;
use crate::logger::{debug, error, info};
use crate::persist::{MicrovmState, MicrovmStateError};
use crate::resources::VmResources;
use crate::seccomp::BpfThreadMap;
use crate::snapshot::Persist;
use crate::vmm_config::instance_info::InstanceInfo;
use crate::vmm_config::machine_config::MachineConfigError;
use crate::vstate::kvm::Kvm;
use crate::vstate::memory::{GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};
use crate::vstate::vcpu::{Vcpu, VcpuError};
use crate::vstate::vm::Vm;
use crate::{AddressManager, EventManager, Vmm, VmmError, device_manager};

/// Errors associated with starting the instance.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum StartMicrovmError {
    /// Unable to attach block device to Vmm: {0}
    AttachBlockDevice(io::Error),
    /// Unable to attach the VMGenID device: {0}
    AttachVmgenidDevice(kvm_ioctls::Error),
    /// System configuration error: {0}
    ConfigureSystem(#[from] ConfigurationError),
    /// Failed to create guest config: {0}
    CreateGuestConfig(#[from] GuestConfigError),
    /// Cannot create network device: {0}
    CreateNetDevice(crate::devices::virtio::net::NetError),
    /// Cannot create RateLimiter: {0}
    CreateRateLimiter(io::Error),
    /// Error creating legacy device: {0}
    #[cfg(target_arch = "x86_64")]
    CreateLegacyDevice(device_manager::legacy::LegacyDeviceError),
    /// Error creating VMGenID device: {0}
    CreateVMGenID(VmGenIdError),
    /// Invalid Memory Configuration: {0}
    GuestMemory(crate::vstate::memory::MemoryError),
    /// Error with initrd initialization: {0}.
    Initrd(#[from] InitrdError),
    /// Internal error while starting microVM: {0}
    Internal(#[from] VmmError),
    /// Failed to get CPU template: {0}
    GetCpuTemplate(#[from] GetCpuTemplateError),
    /// Invalid kernel command line: {0}
    KernelCmdline(String),
    /// Cannot load command line string: {0}
    LoadCommandline(linux_loader::loader::Error),
    /// Cannot start microvm without kernel configuration.
    MissingKernelConfig,
    /// Cannot start microvm without guest mem_size config.
    MissingMemSizeConfig,
    /// No seccomp filter for thread category: {0}
    MissingSeccompFilters(String),
    /// The net device configuration is missing the tap device.
    NetDeviceNotConfigured,
    /// Cannot open the block device backing file: {0}
    OpenBlockDevice(io::Error),
    /// Cannot initialize a MMIO Device or add a device to the MMIO Bus or cmdline: {0}
    RegisterMmioDevice(#[from] device_manager::mmio::MmioError),
    /// Cannot restore microvm state: {0}
    RestoreMicrovmState(MicrovmStateError),
    /// Cannot set vm resources: {0}
    SetVmResources(MachineConfigError),
    /// Cannot create the entropy device: {0}
    CreateEntropyDevice(crate::devices::virtio::rng::EntropyError),
    /// Failed to allocate guest resource: {0}
    AllocateResources(#[from] vm_allocator::Error),
    /// Error starting GDB debug session
    #[cfg(feature = "gdb")]
    GdbServer(gdb::target::GdbTargetError),
    /// Error cloning Vcpu fds
    #[cfg(feature = "gdb")]
    VcpuFdCloneError(#[from] crate::vstate::vcpu::CopyKvmFdError),
    /// Error creating Vfio device
    VfioError(vfio_ioctls::VfioError),
    /// Error setting up Vfio PCI device
    VfioPciError(VfioPciError),
    /// TODO
    Unknown,
}

/// It's convenient to automatically convert `linux_loader::cmdline::Error`s
/// to `StartMicrovmError`s.
impl std::convert::From<linux_loader::cmdline::Error> for StartMicrovmError {
    fn from(err: linux_loader::cmdline::Error) -> StartMicrovmError {
        StartMicrovmError::KernelCmdline(err.to_string())
    }
}

fn create_passthrough_device(vm: &VmFd) -> DeviceFd {
    let mut vfio_dev = kvm_create_device {
        type_: kvm_device_type_KVM_DEV_TYPE_VFIO,
        fd: 0,
        flags: 0,
    };

    vm.create_device(&mut vfio_dev).unwrap()
}

fn register_pci_device_mapping(
    dev: Arc<Mutex<BusDevice>>,
    #[cfg(target_arch = "x86_64")] io_bus: &mut Bus,
    mmio_bus: &mut Bus,
    bars: Vec<PciBarConfiguration>,
) -> Result<(), VmmError> {
    for bar in bars {
        match bar.region_type() {
            PciBarRegionType::IoRegion => {
                #[cfg(target_arch = "x86_64")]
                io_bus
                    .insert(dev.clone(), bar.addr(), bar.size())
                    .map_err(|e| VmmError::DeviceManager(MmioError::BusInsert(e)))?;
                #[cfg(not(target_arch = "x86_64"))]
                error!("I/O region is not supported");
            }
            PciBarRegionType::Memory32BitRegion | PciBarRegionType::Memory64BitRegion => {
                mmio_bus
                    .insert(dev.clone(), bar.addr(), bar.size())
                    .map_err(|e| VmmError::DeviceManager(MmioError::BusInsert(e)))?;
            }
        }
    }
    Ok(())
}

fn add_pci_device(
    bus_device: Arc<Mutex<BusDevice>>,
    pci_segment: &PciSegment,
    dev_manager: &mut MMIODeviceManager,
    pio_manager: &mut PortIODeviceManager,
    allocator: Arc<Mutex<SystemAllocator>>,
    bdf: PciBdf,
) -> Result<(), VmmError> {
    let bars = bus_device
        .lock()
        .unwrap()
        .pci_device_mut()
        .unwrap()
        .allocate_bars(
            &allocator,
            &mut pci_segment.mem32_allocator.lock().unwrap(),
            &mut pci_segment.mem64_allocator.lock().unwrap(),
            None,
        )
        .map_err(|_| VmmError::Unknown)?;

    let mut pci_bus = pci_segment.pci_bus.lock().unwrap();

    pci_bus
        .add_device(bdf.device() as u32, bus_device.clone())
        .map_err(|_| VmmError::Unknown)?;

    register_pci_device_mapping(
        bus_device,
        #[cfg(target_arch = "x86_64")]
        &mut pio_manager.io_bus,
        &mut dev_manager.bus,
        bars.clone(),
    )?;

    Ok(())
}

fn add_vfio_device(
    vmm: &mut Vmm,
    fd: &DeviceFd,
    device_path: &Path,
    memory_slot: Arc<dyn Fn() -> u32 + Send + Sync>,
) -> Result<(), StartMicrovmError> {
    let pci_segment = vmm.pci_segment.as_ref().expect("pci should be enabled");

    // We need to shift the device id since the 3 first bits
    // are dedicated to the PCI function, and we know we don't
    // do multifunction. Also, because we only support one PCI
    // bus, the bus 0, we don't need to add anything to the
    // global device ID.
    let pci_device_id = pci_segment
        .pci_bus
        .lock()
        .expect("bad lock")
        .next_device_id()
        .unwrap();
    let pci_device_bdf = pci_device_id << 3;

    // Safe because we know the RawFd is valid.
    //
    // This dup() is mandatory to be able to give full ownership of the
    // file descriptor to the DeviceFd::from_raw_fd() function later in
    // the code.
    //
    // This is particularly needed so that VfioContainer will still have
    // a valid file descriptor even if DeviceManager, and therefore the
    // passthrough_device are dropped. In case of Drop, the file descriptor
    // would be closed, but Linux would still have the duplicated file
    // descriptor opened from DeviceFd, preventing from unexpected behavior
    // where the VfioContainer would try to use a closed file descriptor.
    let dup_device_fd = unsafe { libc::dup(fd.as_raw_fd()) };

    // SAFETY the raw fd conversion here is safe because:
    //   1. This function is only called on KVM, see the feature guard above.
    //   2. When running on KVM, passthrough_device wraps around DeviceFd.
    //   3. The conversion here extracts the raw fd and then turns the raw fd into a DeviceFd of the
    //      same (correct) type.
    let vfio_container = Arc::new(
        VfioContainer::new(Some(Arc::new(VfioDeviceFd::new_from_kvm(unsafe {
            DeviceFd::from_raw_fd(dup_device_fd)
        }))))
        .map_err(StartMicrovmError::VfioError)?,
    );
    let vfio_device = VfioDevice::new(device_path, Arc::clone(&vfio_container))
        .map_err(StartMicrovmError::VfioError)?;
    info!(
        "Adding VFIO PCI device with ID {} at BDF {} {}",
        pci_device_id,
        pci_device_bdf,
        device_path.display()
    );

    let vfio_pci_device = BusDevice::VfioPciDevice(
        VfioPciDevice::new(
            pci_device_id.to_string(),
            vmm.extra_fd
                .as_ref()
                .expect("pci should be enabled")
                .clone(),
            vfio_device,
            vfio_container.clone(),
            vmm.msi_interrupt_manager
                .as_ref()
                .expect("pci should be enabled")
                .clone(),
            None,
            false,
            pci_device_bdf.into(),
            memory_slot,
            None,
        )
        .unwrap(),
    );

    let vfio_pci_device = Arc::new(Mutex::new(vfio_pci_device));

    add_pci_device(
        vfio_pci_device.clone(),
        pci_segment,
        &mut vmm.mmio_device_manager,
        &mut vmm.pio_device_manager,
        vmm.allocator
            .as_ref()
            .expect("pci should be enabled")
            .clone(),
        pci_device_bdf.into(),
    )
    .unwrap();

    // Register DMA mapping in IOMMU.
    for (_index, region) in vmm.guest_memory.iter().enumerate() {
        info!(
            "Mapping DMA for {:x} len {:x} at hva {:x}",
            region.start_addr().0,
            region.len() as u64,
            // memory.get_host_address(region.start_addr()).unwrap() as u64
            region.as_ptr() as u64
        );
        vfio_pci_device
            .lock()
            .expect("poisoned lock")
            .vfio_pci_device_ref()
            .unwrap()
            .dma_map(
                region.start_addr().0,
                region.len() as u64,
                // memory.get_host_address(region.start_addr()).unwrap() as u64,
                region.as_ptr() as u64,
            )
            .map_err(StartMicrovmError::VfioPciError)?;
    }
    Ok(())
}

// The MMIO address space size is subtracted with 64k. This is done for the
// following reasons:
//  - Reduce the addressable space size by at least 4k to workaround a Linux bug when the VMM
//    allocates devices at the end of the addressable space
//  - Windows requires the addressable space size to be 64k aligned
fn mmio_address_space_size(phys_bits: u8) -> u64 {
    (1 << phys_bits) - (1 << 16)
}

#[cfg_attr(target_arch = "aarch64", allow(unused))]
fn create_vmm_and_vcpus(
    instance_info: &InstanceInfo,
    event_manager: &mut EventManager,
    guest_memory: GuestMemoryMmap,
    uffd: Option<Uffd>,
    vcpu_count: u8,
    kvm_capabilities: Vec<KvmCapability>,
    pci_enabled: bool,
) -> Result<(Vmm, Vec<Vcpu>), VmmError> {
    let kvm = Kvm::new(kvm_capabilities).map_err(VmmError::Kvm)?;
    // Set up Kvm Vm and register memory regions.
    // Build custom CPU config if a custom template is provided.
    let (mut vm, extra_fd) = Vm::new(&kvm)?;
    kvm.check_memory(&guest_memory)?;
    vm.memory_init(&guest_memory)?;

    let (mut vcpus, vcpus_exit_evt) = vm.create_vcpus(vcpu_count).map_err(VmmError::Vm)?;

    let resource_allocator = ResourceAllocator::new()?;

    // Instantiate the MMIO device manager.
    let mut mmio_device_manager = MMIODeviceManager::new();

    // Instantiate ACPI device manager.
    let acpi_device_manager = ACPIDeviceManager::new();

    // For x86_64 we need to create the interrupt controller before calling `KVM_CREATE_VCPUS`
    // while on aarch64 we need to do it the other way around.
    #[cfg(target_arch = "x86_64")]
    let mut pio_device_manager = {
        // Make stdout non blocking.
        set_stdout_nonblocking();

        // Serial device setup.
        let serial_device = setup_serial_device(event_manager, std::io::stdin(), io::stdout())?;

        // x86_64 uses the i8042 reset event as the Vmm exit event.
        let reset_evt = vcpus_exit_evt.try_clone().map_err(VmmError::EventFd)?;

        // TODO Remove these unwraps.
        let mut pio_dev_mgr = PortIODeviceManager::new(serial_device, reset_evt).unwrap();
        pio_dev_mgr
    };

    let (pci_segment, msi_interrupt_manager, allocator, extra_fd) = if pci_enabled {
        // Create a system resources allocator.
        // TODO: use ResourceAllocator
        const NUM_IOAPIC_PINS: usize = 24;
        const X86_64_IRQ_BASE: u32 = 5;

        const PLATFORM_DEVICE_AREA_SIZE: u64 = 1 << 20;
        let end_of_mmio_area = GuestAddress(mmio_address_space_size(46));
        let start_of_device_area = if guest_memory.last_addr() < GuestAddress(MEM_32BIT_RESERVED_START) {
            GuestAddress(1u64 << 32)
        } else {
            guest_memory.last_addr().unchecked_align_up(128 << 20)
        };
        let end_of_device_area = end_of_mmio_area.unchecked_sub(PLATFORM_DEVICE_AREA_SIZE);

        let allocator = Arc::new(Mutex::new(
            SystemAllocator::new(
                #[cfg(target_arch = "x86_64")]
                {
                    GuestAddress(0)
                },
                #[cfg(target_arch = "x86_64")]
                {
                    1 << 16
                },
                end_of_device_area,
                end_of_mmio_area.unchecked_offset_from(end_of_device_area),
                #[cfg(target_arch = "x86_64")]
                vec![GsiApic::new(
                    X86_64_IRQ_BASE,
                    NUM_IOAPIC_PINS as u32 - X86_64_IRQ_BASE,
                )],
            )
            .unwrap(),
        ));

        let vm_fd = Arc::new(Mutex::new(extra_fd));
        // First we create the MSI interrupt manager, the legacy one is created
        // later, after the IOAPIC device creation.
        // The reason we create the MSI one first is because the IOAPIC needs it,
        // and then the legacy interrupt manager needs an IOAPIC. So we're
        // handling a linear dependency chain:
        // msi_interrupt_manager <- IOAPIC <- legacy_interrupt_manager.
        let msi_interrupt_manager: Arc<dyn InterruptManager<GroupConfig = MsiIrqGroupConfig>> =
            Arc::new(MsiInterruptManager::new(
                Arc::clone(&allocator),
                Arc::clone(&vm_fd),
            ));

        // alignment 4 << 10
        let pci_mmio32_allocator = Arc::new(Mutex::new(
            AddressAllocator::new(
                GuestAddress(MEM_32BIT_DEVICES_START),
                MEM_32BIT_DEVICES_SIZE,
            )
            .unwrap(),
        ));

        // alignment 4 << 30
        let pci_mmio64_allocator = Arc::new(Mutex::new(
            AddressAllocator::new(start_of_device_area, end_of_device_area.unchecked_offset_from(start_of_device_area)).unwrap(),
        ));

        // TODO: allocate GSI for legacy interrupts
        // let irqs = resource_allocator.allocate_gsi(8).unwrap();
        // let mut pci_irq_slots: [u8; 32] = [0; 32];
        // for i in 0..32 {
        //     pci_irq_slots[i] = irqs[i % 8] as u8;
        // }
        let pci_irq_slots: [u8; 32] = [(NUM_IOAPIC_PINS - 1) as u8; 32];

        let address_manager = Arc::new(AddressManager {
            allocator: allocator.clone(),
            io_bus: Arc::new(pio_device_manager.io_bus.clone()),
            mmio_bus: Arc::new(mmio_device_manager.bus.clone()),
            vm: vm_fd.clone(),
            pci_mmio32_allocators: vec![pci_mmio32_allocator.clone()],
            pci_mmio64_allocators: vec![pci_mmio64_allocator.clone()],
        });
        let pci_segment = PciSegment::new(
            0,
            0,
            pci_mmio32_allocator,
            pci_mmio64_allocator,
            &mut mmio_device_manager.bus,
            &pci_irq_slots,
            address_manager,
        )
        .unwrap();
        let pci_config_io = Arc::new(Mutex::new(BusDevice::PioPciBus(PciConfigIo::new(
            Arc::clone(&pci_segment.pci_bus),
        ))));
        pio_device_manager.put_pci_bus(pci_config_io);

        (
            Some(pci_segment),
            Some(msi_interrupt_manager),
            Some(allocator),
            Some(vm_fd),
        )
    } else {
        (None, None, None, None)
    };

    pio_device_manager.register_devices(vm.fd()).unwrap();

    let vmm = Vmm {
        events_observer: Some(std::io::stdin()),
        instance_info: instance_info.clone(),
        shutdown_exit_code: None,
        kvm,
        vm,
        guest_memory,
        uffd,
        vcpus_handles: Vec::new(),
        vcpus_exit_evt,
        resource_allocator,
        mmio_device_manager,
        #[cfg(target_arch = "x86_64")]
        pio_device_manager,
        acpi_device_manager,
        extra_fd,
        pci_segment,
        msi_interrupt_manager,
        allocator,
    };

    Ok((vmm, vcpus))
}

/// Builds and starts a microVM based on the current Firecracker VmResources configuration.
///
/// The built microVM and all the created vCPUs start off in the paused state.
/// To boot the microVM and run those vCPUs, `Vmm::resume_vm()` needs to be
/// called.
pub fn build_microvm_for_boot(
    instance_info: &InstanceInfo,
    vm_resources: &super::resources::VmResources,
    event_manager: &mut EventManager,
    seccomp_filters: &BpfThreadMap,
) -> Result<Arc<Mutex<Vmm>>, StartMicrovmError> {
    use self::StartMicrovmError::*;

    // Timestamp for measuring microVM boot duration.
    let request_ts = TimestampUs::default();

    let boot_config = vm_resources
        .boot_source
        .builder
        .as_ref()
        .ok_or(MissingKernelConfig)?;

    let guest_memory = vm_resources
        .allocate_guest_memory()
        .map_err(StartMicrovmError::GuestMemory)?;

    let entry_point = load_kernel(&boot_config.kernel_file, &guest_memory)?;
    let initrd = InitrdConfig::from_config(boot_config, &guest_memory)?;
    // Clone the command-line so that a failed boot doesn't pollute the original.
    #[allow(unused_mut)]
    let mut boot_cmdline = boot_config.cmdline.clone();

    let cpu_template = vm_resources
        .machine_config
        .cpu_template
        .get_cpu_template()?;

    let (mut vmm, mut vcpus) = create_vmm_and_vcpus(
        instance_info,
        event_manager,
        guest_memory,
        None,
        vm_resources.machine_config.vcpu_count,
        cpu_template.kvm_capabilities.clone(),
        vm_resources
            .pci_config
            .as_ref()
            .map(|x| x.enabled)
            .unwrap_or(true),
    )?;

    #[cfg(feature = "gdb")]
    let (gdb_tx, gdb_rx) = mpsc::channel();
    #[cfg(feature = "gdb")]
    vcpus
        .iter_mut()
        .for_each(|vcpu| vcpu.attach_debug_info(gdb_tx.clone()));
    #[cfg(feature = "gdb")]
    let vcpu_fds = vcpus
        .iter()
        .map(|vcpu| vcpu.copy_kvm_vcpu_fd(vmm.vm()))
        .collect::<Result<Vec<_>, _>>()?;

    // The boot timer device needs to be the first device attached in order
    // to maintain the same MMIO address referenced in the documentation
    // and tests.
    let boot_start_timestamp = std::time::Instant::now();
    info!("EGE - microVM boot START timestamp: {:?}", boot_start_timestamp);
    if vm_resources.boot_timer {
        attach_boot_timer_device(&mut vmm, request_ts)?;
    }

    if let Some(balloon) = vm_resources.balloon.get() {
        attach_balloon_device(&mut vmm, &mut boot_cmdline, balloon, event_manager)?;
    }
    info!("Before attaching block devices");
    attach_block_devices(
        &mut vmm,
        &mut boot_cmdline,
        vm_resources.block.devices.iter(),
        event_manager,
    )?;
    info!("After attaching block devices");
    attach_net_devices(
        &mut vmm,
        &mut boot_cmdline,
        vm_resources.net_builder.iter(),
        event_manager,
    )?;

    if let Some(unix_vsock) = vm_resources.vsock.get() {
        attach_unixsock_vsock_device(&mut vmm, &mut boot_cmdline, unix_vsock, event_manager)?;
    }

    if let Some(entropy) = vm_resources.entropy.get() {
        attach_entropy_device(&mut vmm, &mut boot_cmdline, entropy, event_manager)?;
    }


    ///////////////////////////////////////////////////////////////////////////
    ////////////////////// VFIO ////////////////////////////
    ///////////////////////////////////////////////////////////////////////////
    if let Some(vfio_devices) = vm_resources
        .pci_config
        .as_ref()
        .map(|x| x.vfio_devices.as_ref())
        .flatten()
    {
        let device_fd = create_passthrough_device(vmm.vm.fd());
        let memory_slot = Arc::new(move || {
            // TODO use allocator for memory slots
            static mut CURRENT: u32 = 1;
            unsafe {
                CURRENT += 1;
                CURRENT
            }
        });
        for vfio_device in vfio_devices {
            add_vfio_device(
                &mut vmm,
                &device_fd,
                Path::new(&vfio_device.path),
                memory_slot.clone(),
            )?;
        }
    }

    #[cfg(target_arch = "aarch64")]
    attach_legacy_devices_aarch64(event_manager, &mut vmm, &mut boot_cmdline)?;

    attach_vmgenid_device(&mut vmm)?;

    configure_system_for_boot(
        &mut vmm,
        vcpus.as_mut(),
        &vm_resources.machine_config,
        &cpu_template,
        entry_point,
        &initrd,
        boot_cmdline,
    )?;

    let vmm = Arc::new(Mutex::new(vmm));

    #[cfg(feature = "gdb")]
    if let Some(gdb_socket_path) = &vm_resources.machine_config.gdb_socket_path {
        gdb::gdb_thread(
            vmm.clone(),
            vcpu_fds,
            gdb_rx,
            entry_point.entry_addr,
            gdb_socket_path,
        )
        .map_err(GdbServer)?;
    } else {
        debug!("No GDB socket provided not starting gdb server.");
    }

    // Move vcpus to their own threads and start their state machine in the 'Paused' state.
    vmm.lock()
        .unwrap()
        .start_vcpus(
            vcpus,
            seccomp_filters
                .get("vcpu")
                .ok_or_else(|| MissingSeccompFilters("vcpu".to_string()))?
                .clone(),
        )
        .map_err(VmmError::VcpuStart)?;

    // Load seccomp filters for the VMM thread.
    // Execution panics if filters cannot be loaded, use --no-seccomp if skipping filters
    // altogether is the desired behaviour.
    // Keep this as the last step before resuming vcpus.
    crate::seccomp::apply_filter(
        seccomp_filters
            .get("vmm")
            .ok_or_else(|| MissingSeccompFilters("vmm".to_string()))?,
    )
    .map_err(VmmError::SeccompFilters)?;

    event_manager.add_subscriber(vmm.clone());

    Ok(vmm)
}

/// Builds and boots a microVM based on the current Firecracker VmResources configuration.
///
/// This is the default build recipe, one could build other microVM flavors by using the
/// independent functions in this module instead of calling this recipe.
///
/// An `Arc` reference of the built `Vmm` is also plugged in the `EventManager`, while another
/// is returned.
pub fn build_and_boot_microvm(
    instance_info: &InstanceInfo,
    vm_resources: &super::resources::VmResources,
    event_manager: &mut EventManager,
    seccomp_filters: &BpfThreadMap,
) -> Result<Arc<Mutex<Vmm>>, StartMicrovmError> {
    debug!("event_start: build microvm for boot");
    let vmm = build_microvm_for_boot(instance_info, vm_resources, event_manager, seccomp_filters)?;
    debug!("event_end: build microvm for boot");
    // The vcpus start off in the `Paused` state, let them run.
    debug!("event_start: boot microvm");
    vmm.lock().unwrap().resume_vm()?;
    debug!("event_end: boot microvm");
    Ok(vmm)
}

/// Error type for [`build_microvm_from_snapshot`].
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum BuildMicrovmFromSnapshotError {
    /// Failed to create microVM and vCPUs: {0}
    CreateMicrovmAndVcpus(#[from] StartMicrovmError),
    /// Could not access KVM: {0}
    KvmAccess(#[from] vmm_sys_util::errno::Error),
    /// Error configuring the TSC, frequency not present in the given snapshot.
    TscFrequencyNotPresent,
    #[cfg(target_arch = "x86_64")]
    /// Could not get TSC to check if TSC scaling was required with the snapshot: {0}
    GetTsc(#[from] crate::arch::GetTscError),
    #[cfg(target_arch = "x86_64")]
    /// Could not set TSC scaling within the snapshot: {0}
    SetTsc(#[from] crate::arch::SetTscError),
    /// Failed to restore microVM state: {0}
    RestoreState(#[from] crate::vstate::vm::ArchVmError),
    /// Failed to update microVM configuration: {0}
    VmUpdateConfig(#[from] MachineConfigError),
    /// Failed to restore MMIO device: {0}
    RestoreMmioDevice(#[from] MicrovmStateError),
    /// Failed to emulate MMIO serial: {0}
    EmulateSerialInit(#[from] crate::EmulateSerialInitError),
    /// Failed to start vCPUs as no vCPU seccomp filter found.
    MissingVcpuSeccompFilters,
    /// Failed to start vCPUs: {0}
    StartVcpus(#[from] crate::StartVcpusError),
    /// Failed to restore vCPUs: {0}
    RestoreVcpus(#[from] VcpuError),
    /// Failed to apply VMM secccomp filter as none found.
    MissingVmmSeccompFilters,
    /// Failed to apply VMM secccomp filter: {0}
    SeccompFiltersInternal(#[from] crate::seccomp::InstallationError),
    /// Failed to restore ACPI device manager: {0}
    ACPIDeviManager(#[from] ACPIDeviceManagerRestoreError),
    /// VMGenID update failed: {0}
    VMGenIDUpdate(std::io::Error),
}

/// Builds and starts a microVM based on the provided MicrovmState.
///
/// An `Arc` reference of the built `Vmm` is also plugged in the `EventManager`, while another
/// is returned.
#[allow(clippy::too_many_arguments)]
pub fn build_microvm_from_snapshot(
    instance_info: &InstanceInfo,
    event_manager: &mut EventManager,
    microvm_state: MicrovmState,
    guest_memory: GuestMemoryMmap,
    uffd: Option<Uffd>,
    seccomp_filters: &BpfThreadMap,
    vm_resources: &mut VmResources,
) -> Result<Arc<Mutex<Vmm>>, BuildMicrovmFromSnapshotError> {
    // Build Vmm.
    debug!("event_start: build microvm from snapshot");
    let (mut vmm, mut vcpus) = create_vmm_and_vcpus(
        instance_info,
        event_manager,
        guest_memory,
        uffd,
        vm_resources.machine_config.vcpu_count,
        microvm_state.kvm_state.kvm_cap_modifiers.clone(),
        vm_resources
            .pci_config
            .as_ref()
            .map(|x| x.enabled)
            .unwrap_or(false),
    )
    .map_err(StartMicrovmError::Internal)?;

    #[cfg(target_arch = "x86_64")]
    {
        // Scale TSC to match, extract the TSC freq from the state if specified
        if let Some(state_tsc) = microvm_state.vcpu_states[0].tsc_khz {
            // Scale the TSC frequency for all VCPUs. If a TSC frequency is not specified in the
            // snapshot, by default it uses the host frequency.
            if vcpus[0].kvm_vcpu.is_tsc_scaling_required(state_tsc)? {
                for vcpu in &vcpus {
                    vcpu.kvm_vcpu.set_tsc_khz(state_tsc)?;
                }
            }
        }
    }

    // Restore vcpus kvm state.
    for (vcpu, state) in vcpus.iter_mut().zip(microvm_state.vcpu_states.iter()) {
        vcpu.kvm_vcpu
            .restore_state(state)
            .map_err(VcpuError::VcpuResponse)
            .map_err(BuildMicrovmFromSnapshotError::RestoreVcpus)?;
    }

    #[cfg(target_arch = "aarch64")]
    {
        let mpidrs = construct_kvm_mpidrs(&microvm_state.vcpu_states);
        // Restore kvm vm state.
        vmm.vm.restore_state(&mpidrs, &microvm_state.vm_state)?;
    }

    // Restore kvm vm state.
    #[cfg(target_arch = "x86_64")]
    vmm.vm.restore_state(&microvm_state.vm_state)?;

    // Restore the boot source config paths.
    vm_resources.boot_source.config = microvm_state.vm_info.boot_source;

    // Restore devices states.
    let mmio_ctor_args = MMIODevManagerConstructorArgs {
        mem: &vmm.guest_memory,
        vm: vmm.vm.fd(),
        event_manager,
        resource_allocator: &mut vmm.resource_allocator,
        vm_resources,
        instance_id: &instance_info.id,
        restored_from_file: vmm.uffd.is_none(),
    };

    vmm.mmio_device_manager =
        MMIODeviceManager::restore(mmio_ctor_args, &microvm_state.device_states)
            .map_err(MicrovmStateError::RestoreDevices)?;
    vmm.emulate_serial_init()?;

    {
        let acpi_ctor_args = ACPIDeviceManagerConstructorArgs {
            mem: &vmm.guest_memory,
            resource_allocator: &mut vmm.resource_allocator,
            vm: vmm.vm.fd(),
        };

        vmm.acpi_device_manager =
            ACPIDeviceManager::restore(acpi_ctor_args, &microvm_state.acpi_dev_state)?;

        // Inject the notification to VMGenID that we have resumed from a snapshot.
        // This needs to happen before we resume vCPUs, so that we minimize the time between vCPUs
        // resuming and notification being handled by the driver.
        vmm.acpi_device_manager
            .notify_vmgenid()
            .map_err(BuildMicrovmFromSnapshotError::VMGenIDUpdate)?;
    }

    // Move vcpus to their own threads and start their state machine in the 'Paused' state.
    vmm.start_vcpus(
        vcpus,
        seccomp_filters
            .get("vcpu")
            .ok_or(BuildMicrovmFromSnapshotError::MissingVcpuSeccompFilters)?
            .clone(),
    )?;

    let vmm = Arc::new(Mutex::new(vmm));
    event_manager.add_subscriber(vmm.clone());

    // Load seccomp filters for the VMM thread.
    // Keep this as the last step of the building process.
    crate::seccomp::apply_filter(
        seccomp_filters
            .get("vmm")
            .ok_or(BuildMicrovmFromSnapshotError::MissingVmmSeccompFilters)?,
    )?;
    debug!("event_end: build microvm from snapshot");

    Ok(vmm)
}

/// Sets up the serial device.
pub fn setup_serial_device(
    event_manager: &mut EventManager,
    input: std::io::Stdin,
    out: std::io::Stdout,
) -> Result<Arc<Mutex<BusDevice>>, VmmError> {
    let interrupt_evt = EventFdTrigger::new(EventFd::new(EFD_NONBLOCK).map_err(VmmError::EventFd)?);
    let kick_stdin_read_evt =
        EventFdTrigger::new(EventFd::new(EFD_NONBLOCK).map_err(VmmError::EventFd)?);
    let serial = Arc::new(Mutex::new(BusDevice::Serial(SerialWrapper {
        serial: Serial::with_events(
            interrupt_evt,
            SerialEventsWrapper {
                buffer_ready_event_fd: Some(kick_stdin_read_evt),
            },
            SerialOut::Stdout(out),
        ),
        input: Some(input),
    })));
    event_manager.add_subscriber(serial.clone());
    Ok(serial)
}

#[cfg(target_arch = "aarch64")]
fn attach_legacy_devices_aarch64(
    event_manager: &mut EventManager,
    vmm: &mut Vmm,
    cmdline: &mut LoaderKernelCmdline,
) -> Result<(), VmmError> {
    // Serial device setup.
    let cmdline_contains_console = cmdline
        .as_cstring()
        .map_err(|_| VmmError::Cmdline)?
        .into_string()
        .map_err(|_| VmmError::Cmdline)?
        .contains("console=");

    if cmdline_contains_console {
        // Make stdout non-blocking.
        set_stdout_nonblocking();
        let serial = setup_serial_device(event_manager, std::io::stdin(), std::io::stdout())?;
        vmm.mmio_device_manager
            .register_mmio_serial(vmm.vm.fd(), &mut vmm.resource_allocator, serial, None)
            .map_err(VmmError::RegisterMMIODevice)?;
        vmm.mmio_device_manager
            .add_mmio_serial_to_cmdline(cmdline)
            .map_err(VmmError::RegisterMMIODevice)?;
    }

    let rtc = RTCDevice(Rtc::with_events(
        &crate::devices::legacy::rtc_pl031::METRICS,
    ));
    vmm.mmio_device_manager
        .register_mmio_rtc(&mut vmm.resource_allocator, rtc, None)
        .map_err(VmmError::RegisterMMIODevice)
}

/// Attaches a VirtioDevice device to the device manager and event manager.
fn attach_virtio_device<T: 'static + VirtioDevice + MutEventSubscriber + Debug>(
    event_manager: &mut EventManager,
    vmm: &mut Vmm,
    id: String,
    device: Arc<Mutex<T>>,
    cmdline: &mut LoaderKernelCmdline,
    is_vhost_user: bool,
) -> Result<(), MmioError> {
    if vmm.pci_segment.is_some() {
        info!("Attaching VirtioDevice {} as PCI device", id.clone());
        attach_virtio_pci_device(event_manager, vmm, id, device)
    } else {
        info!("Attaching VirtioDevice {} as MMIO device", id.clone());
        attach_virtio_mmio_device(event_manager, vmm, id, device, cmdline, is_vhost_user)
    }
}

/// Attaches a VirtioDevice device to the device manager and event manager.
fn attach_virtio_mmio_device<T: 'static + VirtioDevice + MutEventSubscriber + Debug>(
    event_manager: &mut EventManager,
    vmm: &mut Vmm,
    id: String,
    device: Arc<Mutex<T>>,
    cmdline: &mut LoaderKernelCmdline,
    is_vhost_user: bool,
) -> Result<(), MmioError> {
    event_manager.add_subscriber(device.clone());

    // The device mutex mustn't be locked here otherwise it will deadlock.
    let device = MmioTransport::new(vmm.guest_memory.clone(), device, is_vhost_user);
    vmm.mmio_device_manager
        .register_mmio_virtio_for_boot(
            vmm.vm.fd(),
            &mut vmm.resource_allocator,
            id,
            device,
            cmdline,
        )
        .map(|_| ())
}

fn attach_virtio_pci_device<T: 'static + VirtioDevice + MutEventSubscriber + Debug>(
    event_manager: &mut EventManager,
    vmm: &mut Vmm,
    id: String,
    device: Arc<Mutex<T>>,
) -> Result<(), MmioError> {
    event_manager.add_subscriber(device.clone());
    let pci_segment = vmm.pci_segment.as_ref().expect("pci should be enabled");
    let pci_segment_id = pci_segment.id;
    let pci_device_bdf = pci_segment
        .next_device_bdf()
        .map_err(|_| MmioError::Unknown)?;

    // Allows support for one MSI-X vector per queue. It also adds 1
    // as we need to take into account the dedicated vector to notify
    // about a virtio config change.
    let msix_num = (device.lock().unwrap().queues().len() + 1) as u16;

    let memory = vmm.guest_memory().clone();

    let device_type = device.lock().unwrap().device_type();
    let virtio_pci_device = Arc::new(Mutex::new(BusDevice::VirtioPciDevice(
        VirtioPciDevice::new(
            id.clone(),
            memory,
            device,
            msix_num,
            vmm.msi_interrupt_manager
                .as_ref()
                .expect("pci should be enabled"),
            pci_device_bdf.into(),
            // All device types *except* virtio block devices should be allocated a 64-bit bar
            // The block devices should be given a 32-bit BAR so that they are easily accessible
            // to firmware without requiring excessive identity mapping.
            // The exception being if not on the default PCI segment.
            pci_segment_id > 0 || device_type != virtio::TYPE_BLOCK,
            None,
        )
        .map_err(|_| MmioError::Unknown)?,
    )));

    add_pci_device(
        virtio_pci_device.clone(),
        pci_segment,
        &mut vmm.mmio_device_manager,
        &mut vmm.pio_device_manager,
        vmm.allocator
            .as_ref()
            .expect("pci should be enabled")
            .clone(),
        pci_device_bdf,
    )
    .map_err(|_| MmioError::Unknown)?;

    let bar_addr = virtio_pci_device
        .lock()
        .unwrap()
        .virtio_pci_device_ref()
        .unwrap()
        .config_bar_addr();
    for (i, queue_evt) in virtio_pci_device
        .lock()
        .unwrap()
        .virtio_pci_device_ref()
        .unwrap()
        .virtio_device()
        .lock()
        .unwrap()
        .queue_events()
        .iter()
        .enumerate()
    {
        const NOTIFICATION_BAR_OFFSET: u64 = 0x6000;
        const NOTIFY_OFF_MULTIPLIER: u32 = 4; // A dword per notification address.
        let notify_base = bar_addr + NOTIFICATION_BAR_OFFSET;
        let io_addr =
            IoEventAddress::Mmio(notify_base + i as u64 * u64::from(NOTIFY_OFF_MULTIPLIER));
        vmm.vm
            .fd()
            .register_ioevent(queue_evt, &io_addr, NoDatamatch)
            .map_err(MmioError::RegisterIoEvent)?;
    }

    Ok(())
}

pub(crate) fn attach_boot_timer_device(
    vmm: &mut Vmm,
    request_ts: TimestampUs,
) -> Result<(), MmioError> {
    let boot_timer = crate::devices::pseudo::BootTimer::new(request_ts);

    vmm.mmio_device_manager
        .register_mmio_boot_timer(&mut vmm.resource_allocator, boot_timer)?;

    Ok(())
}

fn attach_vmgenid_device(vmm: &mut Vmm) -> Result<(), StartMicrovmError> {
    let vmgenid = VmGenId::new(&vmm.guest_memory, &mut vmm.resource_allocator)
        .map_err(StartMicrovmError::CreateVMGenID)?;

    vmm.acpi_device_manager
        .attach_vmgenid(vmgenid, vmm.vm.fd())
        .map_err(StartMicrovmError::AttachVmgenidDevice)?;

    Ok(())
}

fn attach_entropy_device(
    vmm: &mut Vmm,
    cmdline: &mut LoaderKernelCmdline,
    entropy_device: &Arc<Mutex<Entropy>>,
    event_manager: &mut EventManager,
) -> Result<(), MmioError> {
    let id = entropy_device
        .lock()
        .expect("Poisoned lock")
        .id()
        .to_string();

    attach_virtio_device(
        event_manager,
        vmm,
        id,
        entropy_device.clone(),
        cmdline,
        false,
    )
}

fn attach_block_devices<'a, I: Iterator<Item = &'a Arc<Mutex<Block>>> + Debug>(
    vmm: &mut Vmm,
    cmdline: &mut LoaderKernelCmdline,
    blocks: I,
    event_manager: &mut EventManager,
) -> Result<(), StartMicrovmError> {
    for block in blocks {
        let (id, is_vhost_user) = {
            let locked = block.lock().expect("Poisoned lock");
            if locked.root_device() {
                match locked.partuuid() {
                    Some(partuuid) => cmdline.insert_str(format!("root=PARTUUID={}", partuuid))?,
                    None => cmdline.insert_str("root=/dev/vda")?,
                }
                match locked.read_only() {
                    true => cmdline.insert_str("ro")?,
                    false => cmdline.insert_str("rw")?,
                }
            }
            (locked.id().to_string(), locked.is_vhost_user())
        };
        // The device mutex mustn't be locked here otherwise it will deadlock.
        info!("Attaching virtio(block) device: {}", id.clone());
        attach_virtio_device(
            event_manager,
            vmm,
            id,
            block.clone(),
            cmdline,
            is_vhost_user,
        )?;
        info!("Virtio(block) device attached");
    }
    Ok(())
}

fn attach_net_devices<'a, I: Iterator<Item = &'a Arc<Mutex<Net>>> + Debug>(
    vmm: &mut Vmm,
    cmdline: &mut LoaderKernelCmdline,
    net_devices: I,
    event_manager: &mut EventManager,
) -> Result<(), StartMicrovmError> {
    for net_device in net_devices {
        let id = net_device.lock().expect("Poisoned lock").id().clone();
        // The device mutex mustn't be locked here otherwise it will deadlock.
        attach_virtio_device(event_manager, vmm, id, net_device.clone(), cmdline, false)?;
    }
    Ok(())
}

fn attach_unixsock_vsock_device(
    vmm: &mut Vmm,
    cmdline: &mut LoaderKernelCmdline,
    unix_vsock: &Arc<Mutex<Vsock<VsockUnixBackend>>>,
    event_manager: &mut EventManager,
) -> Result<(), MmioError> {
    let id = String::from(unix_vsock.lock().expect("Poisoned lock").id());
    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_virtio_device(event_manager, vmm, id, unix_vsock.clone(), cmdline, false)
}

fn attach_balloon_device(
    vmm: &mut Vmm,
    cmdline: &mut LoaderKernelCmdline,
    balloon: &Arc<Mutex<Balloon>>,
    event_manager: &mut EventManager,
) -> Result<(), MmioError> {
    let id = String::from(balloon.lock().expect("Poisoned lock").id());
    // The device mutex mustn't be locked here otherwise it will deadlock.
    attach_virtio_device(event_manager, vmm, id, balloon.clone(), cmdline, false)
}

// Adds `O_NONBLOCK` to the stdout flags.
pub(crate) fn set_stdout_nonblocking() {
    // SAFETY: Call is safe since parameters are valid.
    let flags = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_GETFL, 0) };
    if flags < 0 {
        error!("Could not get Firecracker stdout flags.");
    }
    // SAFETY: Call is safe since parameters are valid.
    let rc = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc < 0 {
        error!("Could not set Firecracker stdout to non-blocking.");
    }
}

#[cfg(test)]
pub(crate) mod tests {

    use linux_loader::cmdline::Cmdline;
    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::arch::DeviceType;
    use crate::device_manager::resources::ResourceAllocator;
    use crate::devices::virtio::block::CacheType;
    use crate::devices::virtio::rng::device::ENTROPY_DEV_ID;
    use crate::devices::virtio::vsock::{TYPE_VSOCK, VSOCK_DEV_ID};
    use crate::devices::virtio::{TYPE_BALLOON, TYPE_BLOCK, TYPE_RNG};
    use crate::mmds::data_store::{Mmds, MmdsVersion};
    use crate::mmds::ns::MmdsNetworkStack;
    use crate::utils::mib_to_bytes;
    use crate::vmm_config::balloon::{BALLOON_DEV_ID, BalloonBuilder, BalloonDeviceConfig};
    use crate::vmm_config::boot_source::DEFAULT_KERNEL_CMDLINE;
    use crate::vmm_config::drive::{BlockBuilder, BlockDeviceConfig};
    use crate::vmm_config::entropy::{EntropyDeviceBuilder, EntropyDeviceConfig};
    use crate::vmm_config::net::{NetBuilder, NetworkInterfaceConfig};
    use crate::vmm_config::vsock::tests::default_config;
    use crate::vmm_config::vsock::{VsockBuilder, VsockDeviceConfig};
    use crate::vstate::vm::tests::setup_vm_with_memory;

    #[derive(Debug)]
    pub(crate) struct CustomBlockConfig {
        drive_id: String,
        is_root_device: bool,
        partuuid: Option<String>,
        is_read_only: bool,
        cache_type: CacheType,
    }

    impl CustomBlockConfig {
        pub(crate) fn new(
            drive_id: String,
            is_root_device: bool,
            partuuid: Option<String>,
            is_read_only: bool,
            cache_type: CacheType,
        ) -> Self {
            CustomBlockConfig {
                drive_id,
                is_root_device,
                partuuid,
                is_read_only,
                cache_type,
            }
        }
    }

    fn cmdline_contains(cmdline: &Cmdline, slug: &str) -> bool {
        // The following unwraps can never fail; the only way any of these methods
        // would return an `Err` is if one of the following conditions is met:
        //    1. The command line is empty: We just added things to it, and if insertion of an
        //       argument goes wrong, then `Cmdline::insert` would have already returned `Err`.
        //    2. There's a spurious null character somewhere in the command line: The
        //       `Cmdline::insert` methods verify that this is not the case.
        //    3. The `CString` is not valid UTF8: It just got created from a `String`, which was
        //       valid UTF8.

        cmdline
            .as_cstring()
            .unwrap()
            .into_string()
            .unwrap()
            .contains(slug)
    }

    pub(crate) fn default_kernel_cmdline() -> Cmdline {
        linux_loader::cmdline::Cmdline::try_from(
            DEFAULT_KERNEL_CMDLINE,
            crate::arch::CMDLINE_MAX_SIZE,
        )
        .unwrap()
    }

    pub(crate) fn default_vmm() -> Vmm {
        let (kvm, mut vm, guest_memory) = setup_vm_with_memory(mib_to_bytes(128));

        let mmio_device_manager = MMIODeviceManager::new();
        let acpi_device_manager = ACPIDeviceManager::new();
        #[cfg(target_arch = "x86_64")]
        let pio_device_manager = PortIODeviceManager::new(
            Arc::new(Mutex::new(BusDevice::Serial(SerialWrapper {
                serial: Serial::with_events(
                    EventFdTrigger::new(EventFd::new(EFD_NONBLOCK).unwrap()),
                    SerialEventsWrapper {
                        buffer_ready_event_fd: None,
                    },
                    SerialOut::Sink(std::io::sink()),
                ),
                input: None,
            }))),
            EventFd::new(libc::EFD_NONBLOCK).unwrap(),
        )
        .unwrap();

        let (_, vcpus_exit_evt) = vm.create_vcpus(1).unwrap();

        Vmm {
            events_observer: Some(std::io::stdin()),
            instance_info: InstanceInfo::default(),
            shutdown_exit_code: None,
            kvm,
            vm,
            guest_memory,
            uffd: None,
            vcpus_handles: Vec::new(),
            vcpus_exit_evt,
            resource_allocator: ResourceAllocator::new().unwrap(),
            mmio_device_manager,
            #[cfg(target_arch = "x86_64")]
            pio_device_manager,
            acpi_device_manager,
        }
    }

    pub(crate) fn insert_block_devices(
        vmm: &mut Vmm,
        cmdline: &mut Cmdline,
        event_manager: &mut EventManager,
        custom_block_cfgs: Vec<CustomBlockConfig>,
    ) -> Vec<TempFile> {
        let mut block_dev_configs = BlockBuilder::new();
        let mut block_files = Vec::new();
        for custom_block_cfg in custom_block_cfgs {
            block_files.push(TempFile::new().unwrap());

            let block_device_config = BlockDeviceConfig {
                drive_id: String::from(&custom_block_cfg.drive_id),
                partuuid: custom_block_cfg.partuuid,
                is_root_device: custom_block_cfg.is_root_device,
                cache_type: custom_block_cfg.cache_type,

                is_read_only: Some(custom_block_cfg.is_read_only),
                path_on_host: Some(
                    block_files
                        .last()
                        .unwrap()
                        .as_path()
                        .to_str()
                        .unwrap()
                        .to_string(),
                ),
                rate_limiter: None,
                file_engine_type: None,

                socket: None,
            };

            block_dev_configs.insert(block_device_config).unwrap();
        }

        attach_block_devices(
            vmm,
            cmdline,
            block_dev_configs.devices.iter(),
            event_manager,
        )
        .unwrap();
        block_files
    }

    pub(crate) fn insert_net_device(
        vmm: &mut Vmm,
        cmdline: &mut Cmdline,
        event_manager: &mut EventManager,
        net_config: NetworkInterfaceConfig,
    ) {
        let mut net_builder = NetBuilder::new();
        net_builder.build(net_config).unwrap();

        let res = attach_net_devices(vmm, cmdline, net_builder.iter(), event_manager);
        res.unwrap();
    }

    pub(crate) fn insert_net_device_with_mmds(
        vmm: &mut Vmm,
        cmdline: &mut Cmdline,
        event_manager: &mut EventManager,
        net_config: NetworkInterfaceConfig,
        mmds_version: MmdsVersion,
    ) {
        let mut net_builder = NetBuilder::new();
        net_builder.build(net_config).unwrap();
        let net = net_builder.iter().next().unwrap();
        let mut mmds = Mmds::default();
        mmds.set_version(mmds_version).unwrap();
        net.lock().unwrap().configure_mmds_network_stack(
            MmdsNetworkStack::default_ipv4_addr(),
            Arc::new(Mutex::new(mmds)),
        );

        attach_net_devices(vmm, cmdline, net_builder.iter(), event_manager).unwrap();
    }

    pub(crate) fn insert_vsock_device(
        vmm: &mut Vmm,
        cmdline: &mut Cmdline,
        event_manager: &mut EventManager,
        vsock_config: VsockDeviceConfig,
    ) {
        let vsock_dev_id = VSOCK_DEV_ID.to_owned();
        let vsock = VsockBuilder::create_unixsock_vsock(vsock_config).unwrap();
        let vsock = Arc::new(Mutex::new(vsock));

        attach_unixsock_vsock_device(vmm, cmdline, &vsock, event_manager).unwrap();

        assert!(
            vmm.mmio_device_manager
                .get_device(DeviceType::Virtio(TYPE_VSOCK), &vsock_dev_id)
                .is_some()
        );
    }

    pub(crate) fn insert_entropy_device(
        vmm: &mut Vmm,
        cmdline: &mut Cmdline,
        event_manager: &mut EventManager,
        entropy_config: EntropyDeviceConfig,
    ) {
        let mut builder = EntropyDeviceBuilder::new();
        let entropy = builder.build(entropy_config).unwrap();

        attach_entropy_device(vmm, cmdline, &entropy, event_manager).unwrap();

        assert!(
            vmm.mmio_device_manager
                .get_device(DeviceType::Virtio(TYPE_RNG), ENTROPY_DEV_ID)
                .is_some()
        );
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn insert_vmgenid_device(vmm: &mut Vmm) {
        attach_vmgenid_device(vmm).unwrap();
        assert!(vmm.acpi_device_manager.vmgenid.is_some());
    }

    pub(crate) fn insert_balloon_device(
        vmm: &mut Vmm,
        cmdline: &mut Cmdline,
        event_manager: &mut EventManager,
        balloon_config: BalloonDeviceConfig,
    ) {
        let mut builder = BalloonBuilder::new();
        builder.set(balloon_config).unwrap();
        let balloon = builder.get().unwrap();

        attach_balloon_device(vmm, cmdline, balloon, event_manager).unwrap();

        assert!(
            vmm.mmio_device_manager
                .get_device(DeviceType::Virtio(TYPE_BALLOON), BALLOON_DEV_ID)
                .is_some()
        );
    }

    #[test]
    fn test_attach_net_devices() {
        let mut event_manager = EventManager::new().expect("Unable to create EventManager");
        let mut vmm = default_vmm();

        let network_interface = NetworkInterfaceConfig {
            iface_id: String::from("netif"),
            host_dev_name: String::from("hostname"),
            guest_mac: None,
            rx_rate_limiter: None,
            tx_rate_limiter: None,
        };

        let mut cmdline = default_kernel_cmdline();
        insert_net_device(
            &mut vmm,
            &mut cmdline,
            &mut event_manager,
            network_interface.clone(),
        );

        // We can not attach it once more.
        let mut net_builder = NetBuilder::new();
        net_builder.build(network_interface).unwrap_err();
    }

    #[test]
    fn test_attach_block_devices() {
        let mut event_manager = EventManager::new().expect("Unable to create EventManager");

        // Use case 1: root block device is not specified through PARTUUID.
        {
            let drive_id = String::from("root");
            let block_configs = vec![CustomBlockConfig::new(
                drive_id.clone(),
                true,
                None,
                true,
                CacheType::Unsafe,
            )];
            let mut vmm = default_vmm();
            let mut cmdline = default_kernel_cmdline();
            insert_block_devices(&mut vmm, &mut cmdline, &mut event_manager, block_configs);
            assert!(cmdline_contains(&cmdline, "root=/dev/vda ro"));
            assert!(
                vmm.mmio_device_manager
                    .get_device(DeviceType::Virtio(TYPE_BLOCK), drive_id.as_str())
                    .is_some()
            );
        }

        // Use case 2: root block device is specified through PARTUUID.
        {
            let drive_id = String::from("root");
            let block_configs = vec![CustomBlockConfig::new(
                drive_id.clone(),
                true,
                Some("0eaa91a0-01".to_string()),
                false,
                CacheType::Unsafe,
            )];
            let mut vmm = default_vmm();
            let mut cmdline = default_kernel_cmdline();
            insert_block_devices(&mut vmm, &mut cmdline, &mut event_manager, block_configs);
            assert!(cmdline_contains(&cmdline, "root=PARTUUID=0eaa91a0-01 rw"));
            assert!(
                vmm.mmio_device_manager
                    .get_device(DeviceType::Virtio(TYPE_BLOCK), drive_id.as_str())
                    .is_some()
            );
        }

        // Use case 3: root block device is not added at all.
        {
            let drive_id = String::from("non_root");
            let block_configs = vec![CustomBlockConfig::new(
                drive_id.clone(),
                false,
                Some("0eaa91a0-01".to_string()),
                false,
                CacheType::Unsafe,
            )];
            let mut vmm = default_vmm();
            let mut cmdline = default_kernel_cmdline();
            insert_block_devices(&mut vmm, &mut cmdline, &mut event_manager, block_configs);
            assert!(!cmdline_contains(&cmdline, "root=PARTUUID="));
            assert!(!cmdline_contains(&cmdline, "root=/dev/vda"));
            assert!(
                vmm.mmio_device_manager
                    .get_device(DeviceType::Virtio(TYPE_BLOCK), drive_id.as_str())
                    .is_some()
            );
        }

        // Use case 4: rw root block device and other rw and ro drives.
        {
            let block_configs = vec![
                CustomBlockConfig::new(
                    String::from("root"),
                    true,
                    Some("0eaa91a0-01".to_string()),
                    false,
                    CacheType::Unsafe,
                ),
                CustomBlockConfig::new(
                    String::from("secondary"),
                    false,
                    None,
                    true,
                    CacheType::Unsafe,
                ),
                CustomBlockConfig::new(
                    String::from("third"),
                    false,
                    None,
                    false,
                    CacheType::Unsafe,
                ),
            ];
            let mut vmm = default_vmm();
            let mut cmdline = default_kernel_cmdline();
            insert_block_devices(&mut vmm, &mut cmdline, &mut event_manager, block_configs);

            assert!(cmdline_contains(&cmdline, "root=PARTUUID=0eaa91a0-01 rw"));
            assert!(
                vmm.mmio_device_manager
                    .get_device(DeviceType::Virtio(TYPE_BLOCK), "root")
                    .is_some()
            );
            assert!(
                vmm.mmio_device_manager
                    .get_device(DeviceType::Virtio(TYPE_BLOCK), "secondary")
                    .is_some()
            );
            assert!(
                vmm.mmio_device_manager
                    .get_device(DeviceType::Virtio(TYPE_BLOCK), "third")
                    .is_some()
            );

            // Check if these three block devices are inserted in kernel_cmdline.
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            assert!(cmdline_contains(
                &cmdline,
                "virtio_mmio.device=4K@0xd0000000:5 virtio_mmio.device=4K@0xd0001000:6 \
                 virtio_mmio.device=4K@0xd0002000:7"
            ));
        }

        // Use case 5: root block device is rw.
        {
            let drive_id = String::from("root");
            let block_configs = vec![CustomBlockConfig::new(
                drive_id.clone(),
                true,
                None,
                false,
                CacheType::Unsafe,
            )];
            let mut vmm = default_vmm();
            let mut cmdline = default_kernel_cmdline();
            insert_block_devices(&mut vmm, &mut cmdline, &mut event_manager, block_configs);
            assert!(cmdline_contains(&cmdline, "root=/dev/vda rw"));
            assert!(
                vmm.mmio_device_manager
                    .get_device(DeviceType::Virtio(TYPE_BLOCK), drive_id.as_str())
                    .is_some()
            );
        }

        // Use case 6: root block device is ro, with PARTUUID.
        {
            let drive_id = String::from("root");
            let block_configs = vec![CustomBlockConfig::new(
                drive_id.clone(),
                true,
                Some("0eaa91a0-01".to_string()),
                true,
                CacheType::Unsafe,
            )];
            let mut vmm = default_vmm();
            let mut cmdline = default_kernel_cmdline();
            insert_block_devices(&mut vmm, &mut cmdline, &mut event_manager, block_configs);
            assert!(cmdline_contains(&cmdline, "root=PARTUUID=0eaa91a0-01 ro"));
            assert!(
                vmm.mmio_device_manager
                    .get_device(DeviceType::Virtio(TYPE_BLOCK), drive_id.as_str())
                    .is_some()
            );
        }

        // Use case 7: root block device is rw with flush enabled
        {
            let drive_id = String::from("root");
            let block_configs = vec![CustomBlockConfig::new(
                drive_id.clone(),
                true,
                None,
                false,
                CacheType::Writeback,
            )];
            let mut vmm = default_vmm();
            let mut cmdline = default_kernel_cmdline();
            insert_block_devices(&mut vmm, &mut cmdline, &mut event_manager, block_configs);
            assert!(cmdline_contains(&cmdline, "root=/dev/vda rw"));
            assert!(
                vmm.mmio_device_manager
                    .get_device(DeviceType::Virtio(TYPE_BLOCK), drive_id.as_str())
                    .is_some()
            );
        }
    }

    #[test]
    fn test_attach_boot_timer_device() {
        let mut vmm = default_vmm();
        let request_ts = TimestampUs::default();

        let res = attach_boot_timer_device(&mut vmm, request_ts);
        res.unwrap();
        assert!(
            vmm.mmio_device_manager
                .get_device(DeviceType::BootTimer, &DeviceType::BootTimer.to_string())
                .is_some()
        );
    }

    #[test]
    fn test_attach_balloon_device() {
        let mut event_manager = EventManager::new().expect("Unable to create EventManager");
        let mut vmm = default_vmm();

        let balloon_config = BalloonDeviceConfig {
            amount_mib: 0,
            deflate_on_oom: false,
            stats_polling_interval_s: 0,
        };

        let mut cmdline = default_kernel_cmdline();
        insert_balloon_device(&mut vmm, &mut cmdline, &mut event_manager, balloon_config);
        // Check if the vsock device is described in kernel_cmdline.
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        assert!(cmdline_contains(
            &cmdline,
            "virtio_mmio.device=4K@0xd0000000:5"
        ));
    }

    #[test]
    fn test_attach_entropy_device() {
        let mut event_manager = EventManager::new().expect("Unable to create EventManager");
        let mut vmm = default_vmm();

        let entropy_config = EntropyDeviceConfig::default();

        let mut cmdline = default_kernel_cmdline();
        insert_entropy_device(&mut vmm, &mut cmdline, &mut event_manager, entropy_config);
        // Check if the vsock device is described in kernel_cmdline.
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        assert!(cmdline_contains(
            &cmdline,
            "virtio_mmio.device=4K@0xd0000000:5"
        ));
    }

    #[test]
    fn test_attach_vsock_device() {
        let mut event_manager = EventManager::new().expect("Unable to create EventManager");
        let mut vmm = default_vmm();

        let mut tmp_sock_file = TempFile::new().unwrap();
        tmp_sock_file.remove().unwrap();
        let vsock_config = default_config(&tmp_sock_file);

        let mut cmdline = default_kernel_cmdline();
        insert_vsock_device(&mut vmm, &mut cmdline, &mut event_manager, vsock_config);
        // Check if the vsock device is described in kernel_cmdline.
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        assert!(cmdline_contains(
            &cmdline,
            "virtio_mmio.device=4K@0xd0000000:5"
        ));
    }
}
