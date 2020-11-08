use super::{
    ap::{
        valid_access_ports, APAccess, APClass, APRegister, AccessPort, BaseaddrFormat, DataSize,
        GenericAP, MemoryAP, BASE, BASE2, CSW, IDR,
    },
    dp::{
        Abort, Ctrl, DPAccess, DPBankSel, DPRegister, DebugPortError, DebugPortId,
        DebugPortVersion, Select, DPIDR,
    },
    memory::{ADIMemoryInterface, Component},
    SwoAccess, SwoConfig,
};
use crate::{
    CommunicationInterface, DebugProbe, DebugProbeError, Error as ProbeRsError, Memory, Probe,
};
use anyhow::anyhow;
use jep106::JEP106Code;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DapError {
    #[error("An error occured in the SWD communication between DAPlink and device.")]
    SwdProtocol,
    #[error("Target device did not respond to request.")]
    NoAcknowledge,
    #[error("Target device responded with FAULT response to request.")]
    FaultResponse,
    #[error("Target device responded with WAIT response to request.")]
    WaitResponse,
    #[error("Target power-up failed.")]
    TargetPowerUpFailed,
    #[error("Incorrect parity on READ request.")]
    IncorrectParity,
}

impl From<DapError> for DebugProbeError {
    fn from(error: DapError) -> Self {
        DebugProbeError::ArchitectureSpecific(Box::new(error))
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum PortType {
    DebugPort,
    AccessPort(u16),
}

impl From<u16> for PortType {
    fn from(value: u16) -> PortType {
        if value == 0xFFFF {
            PortType::DebugPort
        } else {
            PortType::AccessPort(value)
        }
    }
}

impl From<PortType> for u16 {
    fn from(value: PortType) -> u16 {
        match value {
            PortType::DebugPort => 0xFFFF,
            PortType::AccessPort(value) => value,
        }
    }
}
use std::{fmt::Debug, time::Duration};

pub trait Register: Clone + From<u32> + Into<u32> + Sized + Debug {
    const ADDRESS: u8;
    const NAME: &'static str;
}

pub trait DAPAccess: DebugProbe {
    /// Reads the DAP register on the specified port and address
    fn read_register(&mut self, port: PortType, addr: u16) -> Result<u32, DebugProbeError>;

    /// Read multiple values from the same DAP register.
    ///
    /// If possible, this uses optimized read functions, otherwise it
    /// falls back to the `read_register` function.
    fn read_block(
        &mut self,
        port: PortType,
        addr: u16,
        values: &mut [u32],
    ) -> Result<(), DebugProbeError> {
        for val in values {
            *val = self.read_register(port, addr)?;
        }

        Ok(())
    }

    /// Writes a value to the DAP register on the specified port and address
    fn write_register(
        &mut self,
        port: PortType,
        addr: u16,
        value: u32,
    ) -> Result<(), DebugProbeError>;

    /// Write multiple values to the same DAP register.
    ///
    /// If possible, this uses optimized write functions, otherwise it
    /// falls back to the `write_register` function.
    fn write_block(
        &mut self,
        port: PortType,
        addr: u16,
        values: &[u32],
    ) -> Result<(), DebugProbeError> {
        for val in values {
            self.write_register(port, addr, *val)?;
        }

        Ok(())
    }

    /// Flush any outstanding writes.
    ///
    /// By default, this does nothing -- but in probes that implement write
    /// batching, this needs to flush any pending writes.
    fn flush(&mut self) -> Result<(), DebugProbeError> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct ArmCommunicationInterfaceState {
    initialized: bool,

    debug_port_version: DebugPortVersion,

    current_dpbanksel: u8,

    current_apsel: u8,
    current_apbanksel: u8,

    /// Information about the APs of the target.
    /// APs are identified by a number, starting from zero.
    ap_information: Vec<ApInformation>,
}

#[derive(Debug)]
pub enum ApInformation {
    /// Information about a Memory AP, which allows access to target memory. See Chapter C2 in the [ARM Debug Interface Architecture Specification].
    ///
    /// [ARM Debug Interface Architecture Specification]: https://developer.arm.com/documentation/ihi0031/d/
    MemoryAp {
        /// Zero-based port number of the access port. This is used in the debug port to select an AP.
        port_number: u8,
        /// Some Memory APs only support 32 bit wide access to data, while others
        /// also support other widths. Based on this, 8 bit data access can either
        /// be performed directly, or has to be done as a 32 bit access.
        only_32bit_data_size: bool,
        /// The Debug Base Address points to either the start of a set of debug register,
        /// or a ROM table which describes the connected debug components.
        ///
        /// See chapter C2.6, [ARM Debug Interface Architecture Specification].
        ///
        /// [ARM Debug Interface Architecture Specification]: https://developer.arm.com/documentation/ihi0031/d/
        debug_base_address: u64,
    },
    /// Information about an AP with an unknown class.
    Other {
        /// Zero-based port number of the access port. This is used in the debug port to select an AP.
        port_number: u8,
    },
}

impl ArmCommunicationInterfaceState {
    pub fn new() -> Self {
        Self {
            initialized: false,
            debug_port_version: DebugPortVersion::Unsupported(0xFF),
            current_dpbanksel: 0,
            current_apsel: 0,
            current_apbanksel: 0,
            ap_information: Vec::new(),
        }
    }

    pub(crate) fn initialize(&mut self) {
        self.initialized = true;
    }

    pub(crate) fn initialized(&self) -> bool {
        self.initialized
    }
}

#[derive(Debug)]
pub struct ArmCommunicationInterface<'probe> {
    probe: &'probe mut Probe,
    state: &'probe mut ArmCommunicationInterfaceState,
}

fn get_debug_port_version(probe: &mut Probe) -> Result<DebugPortVersion, DebugProbeError> {
    let interface = probe
        .get_interface_dap_mut()?
        .ok_or_else(|| DebugProbeError::InterfaceNotAvailable("ARM"))?;

    let dpidr = DPIDR(interface.read_register(PortType::DebugPort, 0)?);

    Ok(DebugPortVersion::from(dpidr.version()))
}

impl<'probe> ArmCommunicationInterface<'probe> {
    pub fn new(
        probe: &'probe mut Probe,
        state: &'probe mut ArmCommunicationInterfaceState,
    ) -> Result<Option<Self>, DebugProbeError> {
        if probe.has_dap_interface() {
            let mut interface = Self { probe, state };

            if !interface.state.initialized() {
                interface.enter_debug_mode()?;

                /* determine the number and type of available APs */

                for ap in valid_access_ports(&mut interface) {
                    let ap_state = interface.read_ap_information(ap)?;

                    log::debug!("AP {}: {:?}", ap.port_number(), ap_state);

                    interface.state.ap_information.push(ap_state);
                }

                interface.state.initialize();
            }

            Ok(Some(interface))
        } else {
            log::debug!("No DAP interface available on probe");

            Ok(None)
        }
    }

    /// Reborrows the `ArmCommunicationInterface` at hand.
    /// This borrows the references inside the interface and hands them out with a new interface.
    /// This method replaces the normally called `::clone()` method which consumes the object,
    /// which is not what we want.
    pub fn reborrow(&mut self) -> ArmCommunicationInterface<'_> {
        ArmCommunicationInterface::new(self.probe, self.state)
            .unwrap()
            .unwrap()
    }

    pub fn dedicated_memory_interface(&self) -> Result<Option<Memory<'_>>, DebugProbeError> {
        self.probe.dedicated_memory_interface()
    }

    fn enter_debug_mode(&mut self) -> Result<(), DebugProbeError> {
        // Assume that we have DebugPort v1 Interface!
        // Maybe change this in the future when other versions are released.

        // Check the version of debug port used
        let debug_port_version = get_debug_port_version(&mut self.probe)?;
        self.state.debug_port_version = debug_port_version;
        log::debug!("Debug Port version: {:?}", debug_port_version);

        // Read the DP ID.
        let dp_id: DPIDR = self.read_dp_register()?;
        let dp_id: DebugPortId = dp_id.into();
        log::debug!("DebugPort ID:  {:#x?}", dp_id);

        // Clear all existing sticky errors.
        let mut abort_reg = Abort(0);
        abort_reg.set_orunerrclr(true);
        abort_reg.set_wderrclr(true);
        abort_reg.set_stkerrclr(true);
        abort_reg.set_stkcmpclr(true);
        self.write_dp_register(abort_reg)?;

        // Select the DPBANK[0].
        // This is most likely not required but still good practice.
        let mut select_reg = Select(0);
        select_reg.set_dp_bank_sel(0);
        self.write_dp_register(select_reg)?; // select DBPANK 0

        // Power up the system, such that we can actually work with it!
        log::debug!("Requesting debug power");
        let mut ctrl_reg = Ctrl::default();
        ctrl_reg.set_csyspwrupreq(true);
        ctrl_reg.set_cdbgpwrupreq(true);
        self.write_dp_register(ctrl_reg)?;

        // Check the return value to see whether power up was ok.
        let ctrl_reg: Ctrl = self.read_dp_register()?;
        if !(ctrl_reg.csyspwrupack() && ctrl_reg.cdbgpwrupack()) {
            log::error!("Debug power request failed");
            return Err(DapError::TargetPowerUpFailed.into());
        }

        Ok(())
    }

    fn select_ap_and_ap_bank(&mut self, port: u8, ap_bank: u8) -> Result<(), DebugProbeError> {
        let mut cache_changed = if self.state.current_apsel != port {
            self.state.current_apsel = port;
            true
        } else {
            false
        };

        if self.state.current_apbanksel != ap_bank {
            self.state.current_apbanksel = ap_bank;
            cache_changed = true;
        }

        if cache_changed {
            let mut select = Select(0);

            log::debug!(
                "Changing AP to {}, AP_BANK_SEL to {}",
                self.state.current_apsel,
                self.state.current_apbanksel
            );

            select.set_ap_sel(self.state.current_apsel);
            select.set_ap_bank_sel(self.state.current_apbanksel);
            select.set_dp_bank_sel(self.state.current_dpbanksel);

            self.write_dp_register(select)?;
        }

        Ok(())
    }

    fn select_dp_bank(&mut self, dp_bank: DPBankSel) -> Result<(), DebugPortError> {
        match dp_bank {
            DPBankSel::Bank(new_bank) => {
                if new_bank != self.state.current_dpbanksel {
                    self.state.current_dpbanksel = new_bank;

                    let mut select = Select(0);

                    log::debug!("Changing DP_BANK_SEL to {}", self.state.current_dpbanksel);

                    select.set_ap_sel(self.state.current_apsel);
                    select.set_ap_bank_sel(self.state.current_apbanksel);
                    select.set_dp_bank_sel(self.state.current_dpbanksel);

                    self.write_dp_register(select)?;
                }
            }
            DPBankSel::DontCare => (),
        }

        Ok(())
    }

    /// Write the given register `R` of the given `AP`, where the to be written register value
    /// is wrapped in the given `register` parameter.
    pub fn write_ap_register<AP, R>(
        &mut self,
        port: impl Into<AP>,
        register: R,
    ) -> Result<(), DebugProbeError>
    where
        AP: AccessPort,
        R: APRegister<AP>,
    {
        let register_value = register.into();

        log::debug!(
            "Writing register {}, value=0x{:08X}",
            R::NAME,
            register_value
        );

        self.select_ap_and_ap_bank(port.into().port_number(), R::APBANKSEL)?;

        let interface = self
            .probe
            .get_interface_dap_mut()?
            .ok_or_else(|| DebugProbeError::InterfaceNotAvailable("ARM"))?;

        interface.write_register(
            PortType::AccessPort(u16::from(self.state.current_apsel)),
            u16::from(R::ADDRESS),
            register_value,
        )?;
        Ok(())
    }

    // TODO: Fix this ugly: _register: R, values: &[u32]
    /// Write the given register `R` of the given `AP` repeatedly, where the to be written register
    /// values are stored in the `values` array. The values are written in the exact order they are
    /// stored in the array.
    pub fn write_ap_register_repeated<AP, R>(
        &mut self,
        port: impl Into<AP>,
        _register: R,
        values: &[u32],
    ) -> Result<(), DebugProbeError>
    where
        AP: AccessPort,
        R: APRegister<AP>,
    {
        log::debug!(
            "Writing register {}, block with len={} words",
            R::NAME,
            values.len(),
        );

        self.select_ap_and_ap_bank(port.into().port_number(), R::APBANKSEL)?;

        let interface = self
            .probe
            .get_interface_dap_mut()?
            .ok_or_else(|| DebugProbeError::InterfaceNotAvailable("ARM"))?;

        interface.write_block(
            PortType::AccessPort(u16::from(self.state.current_apsel)),
            u16::from(R::ADDRESS),
            values,
        )?;
        Ok(())
    }

    /// Read the given register `R` of the given `AP`, where the read register value is wrapped in
    /// the given `register` parameter.
    pub fn read_ap_register<AP, R>(
        &mut self,
        port: impl Into<AP>,
        _register: R,
    ) -> Result<R, DebugProbeError>
    where
        AP: AccessPort,
        R: APRegister<AP>,
    {
        log::debug!("Reading register {}", R::NAME);
        self.select_ap_and_ap_bank(port.into().port_number(), R::APBANKSEL)?;

        let interface = self
            .probe
            .get_interface_dap_mut()?
            .ok_or_else(|| DebugProbeError::InterfaceNotAvailable("ARM"))?;

        let result = interface.read_register(
            PortType::AccessPort(u16::from(self.state.current_apsel)),
            u16::from(R::ADDRESS),
        )?;

        log::debug!("Read register    {}, value=0x{:08x}", R::NAME, result);

        Ok(R::from(result))
    }

    // TODO: fix types, see above!
    /// Read the given register `R` of the given `AP` repeatedly, where the read register values
    /// are stored in the `values` array. The values are read in the exact order they are stored in
    /// the array.
    pub fn read_ap_register_repeated<AP, R>(
        &mut self,
        port: impl Into<AP>,
        _register: R,
        values: &mut [u32],
    ) -> Result<(), DebugProbeError>
    where
        AP: AccessPort,
        R: APRegister<AP>,
    {
        log::debug!(
            "Reading register {}, block with len={} words",
            R::NAME,
            values.len(),
        );

        self.select_ap_and_ap_bank(port.into().port_number(), R::APBANKSEL)?;

        let interface = self
            .probe
            .get_interface_dap_mut()?
            .ok_or_else(|| DebugProbeError::InterfaceNotAvailable("ARM"))?;

        interface.read_block(
            PortType::AccessPort(u16::from(self.state.current_apsel)),
            u16::from(R::ADDRESS),
            values,
        )?;
        Ok(())
    }

    /// Determine the type and additional information about a AP
    pub(crate) fn ap_information(&self, access_port: GenericAP) -> Option<&ApInformation> {
        self.state
            .ap_information
            .get(access_port.port_number() as usize)
    }

    /// Read information about an AP from its registers.
    ///
    /// This reads the IDR register of the AP, and parses
    /// further AP specific information based on its class.
    ///
    /// Currently, AP specific information is read for Memory APs.
    fn read_ap_information(
        &mut self,
        access_port: GenericAP,
    ) -> Result<ApInformation, DebugProbeError> {
        let idr = self.read_ap_register(access_port, IDR::default())?;

        if idr.CLASS == APClass::MEMAP {
            let access_port: MemoryAP = access_port.into();

            let base_register = self.read_ap_register(access_port, BASE::default())?;

            let mut base_address = if BaseaddrFormat::ADIv5 == base_register.Format {
                let base2 = self.read_ap_register(access_port, BASE2::default())?;

                u64::from(base2.BASEADDR) << 32
            } else {
                0
            };
            base_address |= u64::from(base_register.BASEADDR << 12);

            let only_32bit_data_size = ap_supports_only_32bit_access(self, access_port)?;

            Ok(ApInformation::MemoryAp {
                port_number: access_port.port_number(),
                only_32bit_data_size,
                debug_base_address: base_address,
            })
        } else {
            Ok(ApInformation::Other {
                port_number: access_port.port_number(),
            })
        }
    }
}

impl<'probe> CommunicationInterface for ArmCommunicationInterface<'probe> {
    fn flush(&mut self) -> Result<(), DebugProbeError> {
        let interface = self
            .probe
            .get_interface_dap_mut()?
            .ok_or_else(|| DebugProbeError::InterfaceNotAvailable("ARM"))?;

        interface.flush()
    }
}

impl<'probe> DPAccess for ArmCommunicationInterface<'probe> {
    fn read_dp_register<R: DPRegister>(&mut self) -> Result<R, DebugPortError> {
        if R::VERSION > self.state.debug_port_version {
            return Err(DebugPortError::UnsupportedRegister {
                register: R::NAME,
                version: self.state.debug_port_version,
            });
        }

        self.select_dp_bank(R::DP_BANK)?;

        let interface = self.probe.get_interface_dap_mut()?.ok_or_else(|| {
            DebugPortError::DebugProbe(anyhow!("Could not get interface DAP").into())
        })?;

        log::debug!("Reading DP register {}", R::NAME);
        let result = interface.read_register(PortType::DebugPort, u16::from(R::ADDRESS))?;

        log::debug!("Read    DP register {}, value=0x{:08x}", R::NAME, result);

        Ok(result.into())
    }

    fn write_dp_register<R: DPRegister>(&mut self, register: R) -> Result<(), DebugPortError> {
        if R::VERSION > self.state.debug_port_version {
            return Err(DebugPortError::UnsupportedRegister {
                register: R::NAME,
                version: self.state.debug_port_version,
            });
        }

        self.select_dp_bank(R::DP_BANK)?;

        let interface = self.probe.get_interface_dap_mut()?.ok_or_else(|| {
            DebugPortError::DebugProbe(anyhow!("Could not get interface DAP").into())
        })?;

        let value = register.into();

        log::debug!("Writing DP register {}, value=0x{:08x}", R::NAME, value);
        interface.write_register(PortType::DebugPort, R::ADDRESS as u16, value)?;

        Ok(())
    }
}

impl<'probe> SwoAccess for ArmCommunicationInterface<'probe> {
    fn enable_swo(&mut self, config: &SwoConfig) -> Result<(), ProbeRsError> {
        match self.probe.get_interface_swo_mut() {
            Some(interface) => interface.enable_swo(config),
            None => Err(ProbeRsError::ArchitectureRequired(&["ARMv7", "ARMv8"])),
        }
    }

    fn disable_swo(&mut self) -> Result<(), ProbeRsError> {
        match self.probe.get_interface_swo_mut() {
            Some(interface) => interface.disable_swo(),
            None => Err(ProbeRsError::ArchitectureRequired(&["ARMv7", "ARMv8"])),
        }
    }

    fn read_swo_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>, ProbeRsError> {
        match self.probe.get_interface_swo_mut() {
            Some(interface) => interface.read_swo_timeout(timeout),
            None => Err(ProbeRsError::ArchitectureRequired(&["ARMv7", "ARMv8"])),
        }
    }
}

impl<'probe, R> APAccess<MemoryAP, R> for ArmCommunicationInterface<'probe>
where
    R: APRegister<MemoryAP>,
{
    type Error = DebugProbeError;

    fn read_ap_register(
        &mut self,
        port: impl Into<MemoryAP>,
        register: R,
    ) -> Result<R, Self::Error> {
        self.read_ap_register(port, register)
    }

    fn write_ap_register(
        &mut self,
        port: impl Into<MemoryAP>,
        register: R,
    ) -> Result<(), Self::Error> {
        self.write_ap_register(port, register)
    }

    fn write_ap_register_repeated(
        &mut self,
        port: impl Into<MemoryAP>,
        register: R,
        values: &[u32],
    ) -> Result<(), Self::Error> {
        self.write_ap_register_repeated(port, register, values)
    }

    fn read_ap_register_repeated(
        &mut self,
        port: impl Into<MemoryAP>,
        register: R,
        values: &mut [u32],
    ) -> Result<(), Self::Error> {
        self.read_ap_register_repeated(port, register, values)
    }
}

impl<'probe, R> APAccess<GenericAP, R> for ArmCommunicationInterface<'probe>
where
    R: APRegister<GenericAP>,
{
    type Error = DebugProbeError;

    fn read_ap_register(
        &mut self,
        port: impl Into<GenericAP>,
        register: R,
    ) -> Result<R, Self::Error> {
        self.read_ap_register(port, register)
    }

    fn write_ap_register(
        &mut self,
        port: impl Into<GenericAP>,
        register: R,
    ) -> Result<(), Self::Error> {
        self.write_ap_register(port, register)
    }

    fn write_ap_register_repeated(
        &mut self,
        port: impl Into<GenericAP>,
        register: R,
        values: &[u32],
    ) -> Result<(), Self::Error> {
        self.write_ap_register_repeated(port, register, values)
    }

    fn read_ap_register_repeated(
        &mut self,
        port: impl Into<GenericAP>,
        register: R,
        values: &mut [u32],
    ) -> Result<(), Self::Error> {
        self.read_ap_register_repeated(port, register, values)
    }
}

/// Check that target supports memory access with sizes different from 32 bits.
///
/// If only 32-bit access is supported, the SIZE field will be read-only and changing it
/// will not have any effect.
fn ap_supports_only_32bit_access(
    interface: &mut ArmCommunicationInterface,
    ap: MemoryAP,
) -> Result<bool, DebugProbeError> {
    let mut csw = interface.read_ap_register(ap, CSW::default())?;
    csw.SIZE = DataSize::U8;
    interface.write_ap_register(ap, csw)?;

    let csw = interface.read_ap_register(ap, CSW::default())?;
    Ok(csw.SIZE != DataSize::U8)
}

#[derive(Debug)]
pub struct ArmChipInfo {
    pub manufacturer: JEP106Code,
    pub part: u16,
}

impl ArmChipInfo {
    pub fn read_from_rom_table(
        interface: &mut ArmCommunicationInterface,
    ) -> Result<Option<Self>, ProbeRsError> {
        for access_port in valid_access_ports(interface) {
            let idr = interface
                .read_ap_register(access_port, IDR::default())
                .map_err(ProbeRsError::Probe)?;
            log::debug!("{:#x?}", idr);

            if idr.CLASS == APClass::MEMAP {
                let access_port: MemoryAP = access_port.into();

                let baseaddr = access_port.base_address(interface)?;

                let data_size = ap_supports_only_32bit_access(interface, access_port)?;

                let mut memory = Memory::new(
                    ADIMemoryInterface::<ArmCommunicationInterface>::new(
                        interface.reborrow(),
                        access_port,
                        data_size,
                    )
                    .map_err(ProbeRsError::architecture_specific)?,
                );

                let component = Component::try_parse(&mut memory, baseaddr)
                    .map_err(ProbeRsError::architecture_specific)?;

                if let Component::Class1RomTable(component_id, _) = component {
                    if let Some(jep106) = component_id.peripheral_id().jep106() {
                        return Ok(Some(ArmChipInfo {
                            manufacturer: jep106,
                            part: component_id.peripheral_id().part(),
                        }));
                    }
                }
            }
        }
        // log::info!(
        //     "{}\n{}\n{}\n{}",
        //     "If you are using a Nordic chip, it might be locked to debug access".yellow(),
        //     "Run cargo flash with --nrf-recover to unlock".yellow(),
        //     "WARNING: --nrf-recover will erase the entire code".yellow(),
        //     "flash and UICR area of the device, in addition to the entire RAM".yellow()
        // );

        Ok(None)
    }
}

impl std::fmt::Display for ArmChipInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let manu = match self.manufacturer.get() {
            Some(name) => name.to_string(),
            None => format!(
                "<unknown manufacturer (cc={:2x}, id={:2x})>",
                self.manufacturer.cc, self.manufacturer.id
            ),
        };
        write!(f, "{} 0x{:04x}", manu, self.part)
    }
}
