use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use machine::{AUDIO_SAMPLE_RATE, Machine, Region};
use plugin_api::{
    ABI_MAJOR_V1, ABI_MINOR_V1, AddressSpaceInfo, BinaryBlob, BitmapExplorerDefaults,
    BitmapPaletteColor, ChipSnapshot, CodecError, CpuFamily, DebugCommand, DebugResponse,
    DisassemblyLine, MACHINE_PROFILE_CONTRACT_VERSION, MachineCapability, MachineCommand,
    MachineCreateRequest, MachineCreateResponse, MachineInstanceCallRequest,
    MachineInstanceCallResponse, MachineInstanceHandle, MachineMetadata, MachineOptionInfo,
    MachineOptionKind, MachinePluginVTableV1, MachineProfile, MachineResponse, MediaKind,
    MediaSnapshot, PluginDescriptorV1, PluginKind, REQUIRED_MACHINE_DEBUG_CAPABILITIES, RcBuffer,
    RcCallResult, RcSlice, RcStatus, RegisterField, RegisterSnapshot, StatusCode,
};
use zx81::{Zx81, Zx81Key};

const PLUGIN_ID: &[u8] = b"retroclawd.zx81.machine";

fn bitmap_palette() -> Vec<BitmapPaletteColor> {
    vec![
        BitmapPaletteColor {
            name: "black".to_string(),
            rgb: [0x00, 0x00, 0x00],
        },
        BitmapPaletteColor {
            name: "white".to_string(),
            rgb: [0xFF, 0xFF, 0xFF],
        },
    ]
}

fn bitmap_defaults() -> BitmapExplorerDefaults {
    BitmapExplorerDefaults {
        address: 0x4000,
        bytes_width: 32,
        bitplanes: 1,
        zoom: 3,
    }
}
const PLUGIN_VERSION: &[u8] = env!("CARGO_PKG_VERSION").as_bytes();

const fn static_slice(bytes: &'static [u8]) -> RcSlice {
    RcSlice {
        ptr: bytes.as_ptr(),
        len: bytes.len(),
    }
}

struct StaticPluginDescriptor(PluginDescriptorV1);

// SAFETY: descriptor and vtable refer only to immutable static data/function pointers.
unsafe impl Sync for StaticPluginDescriptor {}

static STATE: OnceLock<Mutex<PluginState>> = OnceLock::new();

static MACHINE_VTABLE_V1: MachinePluginVTableV1 = MachinePluginVTableV1 {
    size: MachinePluginVTableV1::expected_size(),
    list_profiles: plugin_list_profiles_v1,
    create_instance: plugin_create_instance_v1,
    destroy_instance: plugin_destroy_instance_v1,
    instance_call: plugin_instance_call_v1,
    free_buffer: plugin_free_buffer_v1,
    step: plugin_step_v1,
    halted: plugin_halted_v1,
    pc: plugin_pc_v1,
};

static PLUGIN_DESCRIPTOR_V1: StaticPluginDescriptor = StaticPluginDescriptor(PluginDescriptorV1 {
    abi_major: ABI_MAJOR_V1,
    abi_minor: ABI_MINOR_V1,
    kind: PluginKind::Machine,
    id: static_slice(PLUGIN_ID),
    version: static_slice(PLUGIN_VERSION),
    vtable: (&MACHINE_VTABLE_V1 as *const MachinePluginVTableV1).cast(),
});

struct PluginState {
    next_instance: MachineInstanceHandle,
    instances: HashMap<MachineInstanceHandle, InstanceRecord>,
}

struct InstanceRecord {
    machine: Zx81,
    region: String,
    paused: bool,
    breakpoints: Vec<u16>,
}

impl PluginState {
    fn new() -> Self {
        Self {
            next_instance: 1,
            instances: HashMap::new(),
        }
    }

    fn allocate_instance(&mut self) -> MachineInstanceHandle {
        let instance = self.next_instance;
        self.next_instance = self.next_instance.wrapping_add(1).max(1);
        instance
    }

    fn record_mut(
        &mut self,
        instance: MachineInstanceHandle,
    ) -> Result<&mut InstanceRecord, String> {
        self.instances
            .get_mut(&instance)
            .ok_or_else(|| format!("unknown machine instance handle {instance}"))
    }
}

fn state() -> &'static Mutex<PluginState> {
    STATE.get_or_init(|| Mutex::new(PluginState::new()))
}

fn status_from_error(error: &str) -> StatusCode {
    if error.contains("unknown machine instance handle") {
        StatusCode::NotFound
    } else if error.contains("unsupported") {
        StatusCode::Unsupported
    } else {
        StatusCode::InvalidRequest
    }
}

fn decode_request<T: serde::de::DeserializeOwned>(req: RcSlice) -> Result<T, CodecError> {
    // SAFETY: caller owns request bytes for call duration.
    plugin_api::decode_message(unsafe { req.as_bytes() })
}

fn encode_ok<T: serde::Serialize>(value: &T) -> RcCallResult {
    match plugin_api::encode_message(value) {
        Ok(bytes) => RcCallResult::ok_bytes(bytes),
        Err(e) => e.into_call_result(),
    }
}

fn machine_profile() -> MachineProfile {
    MachineProfile {
        machine_id: "zx81".to_string(),
        display_name: "Sinclair ZX81 (plugin)".to_string(),
        cpu_family: CpuFamily::Z80,
        default_region: Some("pal".to_string()),
        contract_version: MACHINE_PROFILE_CONTRACT_VERSION,
        debug_capabilities: REQUIRED_MACHINE_DEBUG_CAPABILITIES.to_vec(),
        capabilities: vec![
            MachineCapability::Video,
            MachineCapability::Snapshot,
            MachineCapability::SaveState,
            MachineCapability::Debug,
        ],
        media_kinds: vec![MediaKind::Snapshot],
        options: vec![
            MachineOptionInfo {
                key: "rom".to_string(),
                flag: "rom".to_string(),
                short: None,
                kind: MachineOptionKind::File,
                default: None,
                required: true,
                description: "8K ZX81 ROM image".to_string(),
            },
            MachineOptionInfo {
                key: "ram_kb".to_string(),
                flag: "ram-kb".to_string(),
                short: None,
                kind: MachineOptionKind::Enum {
                    choices: zx81::RAM_KB_CHOICES
                        .iter()
                        .map(|kb| kb.to_string())
                        .collect(),
                },
                default: Some(plugin_api::DebugValue::Text("1".to_string())),
                required: false,
                description:
                    "RAM installed, in K: the on-board 1K, a 2K upgrade, or a 16K/32K/48K/56K pack"
                        .to_string(),
            },
        ],
        key_names: key_names(),
    }
}

fn parse_region(region: Option<&str>) -> Result<Region, String> {
    match region.map(|s| s.to_ascii_lowercase()) {
        None => Ok(Region::Pal),
        Some(value) if value == "pal" => Ok(Region::Pal),
        Some(value) if value == "ntsc" => Ok(Region::Ntsc),
        Some(value) if value == "japan" => Ok(Region::Japan),
        Some(other) => Err(format!("unsupported region \"{other}\"")),
    }
}

fn blob<'a>(roms: &'a [BinaryBlob], key: &str) -> Option<&'a [u8]> {
    roms.iter()
        .find(|blob| blob.key.eq_ignore_ascii_case(key))
        .map(|blob| blob.bytes.as_slice())
}

fn create_machine(req: MachineCreateRequest) -> Result<MachineCreateResponse, String> {
    if !req.machine_id.eq_ignore_ascii_case("zx81") {
        return Err(format!(
            "unsupported machine id \"{}\" (plugin supports only zx81)",
            req.machine_id
        ));
    }

    let rom =
        blob(&req.roms, "rom").ok_or_else(|| "missing required binary blob: rom".to_string())?;
    if rom.len() != zx81::ROM_SIZE {
        return Err(format!(
            "rom blob has wrong size: expected {} bytes, got {}",
            zx81::ROM_SIZE,
            rom.len()
        ));
    }

    let mut ram_kb = 1usize;
    for option in &req.options {
        match (option.key.as_str(), &option.value) {
            ("ram_kb", plugin_api::DebugValue::Text(kb)) => {
                ram_kb = kb.parse().map_err(|_| {
                    format!("option \"ram_kb\" must be a number of K, got \"{kb}\"")
                })?;
            }
            (key, _) => {
                return Err(format!(
                    "unsupported machine-create option \"{key}\" for zx81 plugin"
                ));
            }
        }
    }

    let region = parse_region(req.region.as_deref())?;
    let mut state = state()
        .lock()
        .map_err(|_| "plugin state mutex poisoned".to_string())?;
    let instance = state.allocate_instance();
    state.instances.insert(
        instance,
        InstanceRecord {
            machine: Zx81::with_ram_kb(rom.to_vec(), region, ram_kb)?,
            region: region_name(region).to_string(),
            paused: false,
            breakpoints: Vec::new(),
        },
    );

    Ok(MachineCreateResponse {
        instance,
        metadata: MachineMetadata {
            machine_id: "zx81".to_string(),
            model: Some("zx81".to_string()),
            region: Some(region_name(region).to_string()),
            notes: vec!["external machine plugin v1".to_string()],
            bitmap_palette: bitmap_palette(),
            bitmap_defaults: Some(bitmap_defaults()),
        },
    })
}

fn region_name(region: Region) -> &'static str {
    match region {
        Region::Pal => "pal",
        Region::Ntsc => "ntsc",
        Region::Japan => "japan",
    }
}

fn option_u32(options: &[plugin_api::NamedValue], key: &str) -> Result<Option<u32>, String> {
    let Some(value) = options
        .iter()
        .find(|item| item.key.eq_ignore_ascii_case(key))
        .map(|item| &item.value)
    else {
        return Ok(None);
    };

    match value {
        plugin_api::DebugValue::U64(v) => u32::try_from(*v)
            .map(Some)
            .map_err(|_| format!("option \"{key}\" out of range for u32")),
        _ => Err(format!("option \"{key}\" must be an unsigned integer")),
    }
}

fn handle_machine_command(
    record: &mut InstanceRecord,
    command: MachineCommand,
) -> Result<MachineResponse, String> {
    match command {
        MachineCommand::GetClockHz => Ok(MachineResponse::ClockHz(record.machine.clock_hz())),
        MachineCommand::GetFrameHz => Ok(MachineResponse::FrameHz(record.machine.frame_hz())),
        MachineCommand::RequestInterrupt => {
            record.machine.request_interrupt();
            Ok(MachineResponse::Ack)
        }
        MachineCommand::Step => {
            let cycles = record.machine.step();
            Ok(MachineResponse::Step(plugin_api::StepResult {
                cycles,
                pc: record.machine.pc(),
                halted: record.machine.halted(),
            }))
        }
        MachineCommand::IsHalted => Ok(MachineResponse::Halted(record.machine.halted())),
        MachineCommand::GetPc => Ok(MachineResponse::Pc(record.machine.pc())),
        MachineCommand::Reset => {
            record.machine.reset();
            Ok(MachineResponse::Ack)
        }
        MachineCommand::ReadMemory { address, len, .. } => {
            let bytes = (0..len)
                .map(|offset| record.machine.read_memory(address.wrapping_add(offset)))
                .collect();
            Ok(MachineResponse::Memory(bytes))
        }
        // A `space` request (banked/paged poke) is a clear error here, not
        // a silent flat write -- ZX81 has no banked memory at all (1K RAM,
        // no rampack concept modeled), so honoring an unrecognized `space`
        // by writing flat memory instead would silently poke the wrong
        // place, same reasoning `c64-machine-plugin`'s own `WriteMemory`
        // handler now uses.
        MachineCommand::WriteMemory { space: Some(_), .. } => {
            Err("banked/paged memory writes are not supported for the zx81 plugin".to_string())
        }
        MachineCommand::WriteMemory {
            address,
            bytes,
            space: None,
        }
        | MachineCommand::LoadBytes { address, bytes } => {
            let written = record.machine.load(address, &bytes);
            Ok(MachineResponse::BytesWritten(written as u32))
        }
        MachineCommand::SetPc { address } => {
            record.machine.set_pc(address);
            Ok(MachineResponse::Ack)
        }
        MachineCommand::SaveState => Ok(MachineResponse::StateBlob(record.machine.save_state())),
        MachineCommand::SaveSnapshot { extension } => Err(format!(
            "unsupported snapshot save extension \".{extension}\" for zx81 plugin"
        )),
        MachineCommand::LoadState { bytes } => {
            record.machine.load_state(&bytes)?;
            Ok(MachineResponse::Ack)
        }
        MachineCommand::AttachMedia {
            slot: _,
            kind,
            name,
            bytes,
            options,
        } => {
            let _tape_speed = option_u32(&options, "tape_speed")
                .or_else(|_| option_u32(&options, "speed_multiplier"))?
                .unwrap_or(1);
            if kind != MediaKind::Snapshot {
                return Err(format!(
                    "unsupported media kind for zx81 plugin: {:?} (only snapshot is supported)",
                    kind
                ));
            }
            let extension = snapshot_extension(name.as_deref());
            match extension.as_str() {
                "p" | "81" => {
                    record
                        .machine
                        .load_p_snapshot(zx81_snapshot::parse_p(&bytes));
                    Ok(MachineResponse::Ack)
                }
                "p81" => {
                    let snapshot = zx81_snapshot::parse_p81(&bytes).map_err(|e| e.to_string())?;
                    record.machine.load_p_snapshot(snapshot);
                    Ok(MachineResponse::Ack)
                }
                other => Err(format!(
                    "unsupported snapshot extension for zx81 plugin: .{other} (expected .p, .p81, or .81)"
                )),
            }
        }
        MachineCommand::DetachMedia { slot } => {
            Err(format!("detach media not implemented for slot {:?}", slot))
        }
        MachineCommand::ExportDisk { .. } | MachineCommand::MarkDiskSaved { .. } => {
            Err("unsupported: the zx81 has no writable disk".to_string())
        }
        MachineCommand::ExportTape { .. } => {
            Err("unsupported: the zx81 plugin does not record tape output".to_string())
        }
        MachineCommand::Freeze
        | MachineCommand::ExportCartridge
        | MachineCommand::ExportPrinter => {
            Err("unsupported: the zx81 has no freezer or writable cartridge".to_string())
        }
        MachineCommand::GetMediaSnapshot => Ok(MachineResponse::MediaSnapshot(MediaSnapshot {
            tape: None,
            disks: Vec::new(),
            cartridge: None,
        })),
        MachineCommand::SetBorder { enabled } => {
            record.machine.set_border(enabled);
            Ok(MachineResponse::Ack)
        }
        MachineCommand::KeyEvent { key, pressed } => {
            // A combo is Shift plus a key, pressed and released together (`Plus` is Shift+K).
            let keys: Vec<Zx81Key> = match zx81_combo_from_name(&key) {
                Some(base) => vec![Zx81Key::Shift, base],
                None => zx81_key_from_name(&key).into_iter().collect(),
            };
            for key in keys {
                if pressed {
                    record.machine.press_key(key);
                } else {
                    record.machine.release_key(key);
                }
            }
            Ok(MachineResponse::Ack)
        }
        MachineCommand::MouseMotion { .. }
        | MachineCommand::MouseButton { .. }
        | MachineCommand::MouseWheel { .. } => Ok(MachineResponse::Ack),
        MachineCommand::WriteIo { port, value } => {
            record.machine.write_port(port, value);
            Ok(MachineResponse::Ack)
        }
        MachineCommand::SetPortWatches { watches } => {
            record.machine.set_port_watches(
                watches
                    .into_iter()
                    .map(|w| machine::PortWatch {
                        port: w.port,
                        mask: w.mask,
                        on_read: w.on_read,
                        on_write: w.on_write,
                    })
                    .collect(),
            );
            Ok(MachineResponse::Ack)
        }
        MachineCommand::TakePortHit => Ok(MachineResponse::PortHit(
            record
                .machine
                .take_port_hit()
                .map(|hit| plugin_api::PortHitInfo {
                    port: hit.port,
                    value: hit.value,
                    write: hit.write,
                }),
        )),
        MachineCommand::RenderFrame => {
            let (width, height, rgb) = record
                .machine
                .render_frame()
                .ok_or_else(|| "render_frame unavailable for zx81".to_string())?;
            Ok(MachineResponse::Frame { width, height, rgb })
        }
        MachineCommand::DrainAudio => Ok(MachineResponse::AudioPcmMono {
            sample_rate: AUDIO_SAMPLE_RATE,
            samples: record.machine.drain_audio(),
        }),
        MachineCommand::Debug(command) => {
            let response = handle_debug_command(record, command)?;
            Ok(MachineResponse::Debug(response))
        }
    }
}

fn snapshot_extension(name: Option<&str>) -> String {
    if let Some(name) = name
        && let Some(ext) = name.rsplit('.').next()
    {
        return ext.to_ascii_lowercase();
    }
    "p".to_string()
}

/// Every key name this machine's `KeyEvent` accepts -- built from the same
/// tables the event handler looks names up in, so the list cannot drift.
fn key_names() -> Vec<plugin_api::MachineKeyInfo> {
    let mut infos = plugin_api::key_infos(
        zx81_key_table()
            .iter()
            .map(|(name, _)| *name)
            .chain(zx81_combo_table().iter().map(|(name, _)| *name)),
    );
    plugin_api::set_key_chars(&mut infos, ZX81_CHARS);
    infos
}

/// The ASCII character each key types on a real ZX81 (its Shift legends), for the host's symbolic keyboard layout and for
/// typing text. `£` (Shift+Space) has no ASCII character, so it is a key but types nothing.
const ZX81_CHARS: &[(&str, &str)] = &[
    ("Space", " "),
    ("Period", "."),
    ("Colon", ":"),
    ("Semicolon", ";"),
    ("Question", "?"),
    ("Slash", "/"),
    ("DoubleQuote", "\""),
    ("RightParen", ")"),
    ("LeftParen", "("),
    ("Dollar", "$"),
    ("Equals", "="),
    ("Plus", "+"),
    ("Minus", "-"),
    ("Comma", ","),
    ("GreaterThan", ">"),
    ("LessThan", "<"),
    ("Asterisk", "*"),
];

/// The keys that are Shift plus another key, with the symbol each types. The pairs are read from the ZX81 ROM's own
/// shifted key table (0x00A5): Shift+Z `:`, X `;`, C `?`, V `/`, P `"`, O `)`, I `(`, U `$`, L `=`, K `+`, J `-`,
/// `.` `,`, M `>`, N `<`, B `*`, Space `£`.
fn zx81_combo_table() -> &'static [(&'static str, Zx81Key)] {
    use Zx81Key::*;
    &[
        ("Colon", Z),
        ("Semicolon", X),
        ("Question", C),
        ("Slash", V),
        ("DoubleQuote", P),
        ("RightParen", O),
        ("LeftParen", I),
        ("Dollar", U),
        ("Equals", L),
        ("Plus", K),
        ("Minus", J),
        ("Comma", Dot),
        ("GreaterThan", M),
        ("LessThan", N),
        ("Asterisk", B),
        ("Pound", Space),
    ]
}

fn zx81_combo_from_name(name: &str) -> Option<Zx81Key> {
    zx81_combo_table()
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, value)| *value)
}

fn zx81_key_table() -> &'static [(&'static str, Zx81Key)] {
    use Zx81Key::*;
    &[
        ("KeyA", A),
        ("KeyB", B),
        ("KeyC", C),
        ("KeyD", D),
        ("KeyE", E),
        ("KeyF", F),
        ("KeyG", G),
        ("KeyH", H),
        ("KeyI", I),
        ("KeyJ", J),
        ("KeyK", K),
        ("KeyL", L),
        ("KeyM", M),
        ("KeyN", N),
        ("KeyO", O),
        ("KeyP", P),
        ("KeyQ", Q),
        ("KeyR", R),
        ("KeyS", S),
        ("KeyT", T),
        ("KeyU", U),
        ("KeyV", V),
        ("KeyW", W),
        ("KeyX", X),
        ("KeyY", Y),
        ("KeyZ", Z),
        ("Digit0", Digit0),
        ("Digit1", Digit1),
        ("Digit2", Digit2),
        ("Digit3", Digit3),
        ("Digit4", Digit4),
        ("Digit5", Digit5),
        ("Digit6", Digit6),
        ("Digit7", Digit7),
        ("Digit8", Digit8),
        ("Digit9", Digit9),
        ("Space", Space),
        ("Period", Dot),
        ("Enter", Enter),
        ("ShiftLeft", Shift),
        ("ShiftRight", Shift),
        ("ArrowLeft", Digit5),
        ("ArrowDown", Digit6),
        ("ArrowUp", Digit7),
        ("ArrowRight", Digit8),
    ]
}

fn zx81_key_from_name(name: &str) -> Option<Zx81Key> {
    zx81_key_table()
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, value)| *value)
}

fn register_snapshot(registers: Vec<(&'static str, u64)>) -> RegisterSnapshot {
    let registers = registers
        .into_iter()
        .map(|(name, value)| RegisterField {
            bank: "main".to_string(),
            name: name.to_string(),
            value,
            width_bits: inferred_width_bits(value),
        })
        .collect();

    RegisterSnapshot {
        cpu_family: CpuFamily::Z80,
        registers,
        flags: Vec::new(),
    }
}

fn inferred_width_bits(value: u64) -> u16 {
    if value <= 0xFF {
        8
    } else if value <= 0xFFFF {
        16
    } else if value <= 0xFFFF_FFFF {
        32
    } else {
        64
    }
}

fn handle_debug_command(
    record: &mut InstanceRecord,
    command: DebugCommand,
) -> Result<DebugResponse, String> {
    match command {
        DebugCommand::Pause => {
            record.paused = true;
            Ok(DebugResponse::Ack)
        }
        DebugCommand::Resume => {
            record.paused = false;
            Ok(DebugResponse::Ack)
        }
        DebugCommand::StepInstruction | DebugCommand::StepOver => {
            record.machine.step();
            Ok(DebugResponse::Ack)
        }
        DebugCommand::SetBreakpoint { address } => {
            if !record.breakpoints.contains(&address) {
                record.breakpoints.push(address);
                record.breakpoints.sort_unstable();
            }
            Ok(DebugResponse::Ack)
        }
        DebugCommand::ClearBreakpoint { address } => {
            record.breakpoints.retain(|bp| *bp != address);
            Ok(DebugResponse::Ack)
        }
        DebugCommand::ClearAllBreakpoints => {
            record.breakpoints.clear();
            Ok(DebugResponse::Ack)
        }
        DebugCommand::ListBreakpoints => Ok(DebugResponse::Breakpoints(record.breakpoints.clone())),
        DebugCommand::GetRegisters => Ok(DebugResponse::Registers(register_snapshot(
            record.machine.registers(),
        ))),
        DebugCommand::SetRegister { name, value } => {
            record.machine.set_register(&name, value)?;
            Ok(DebugResponse::Ack)
        }
        DebugCommand::GetAddressSpaces => {
            Ok(DebugResponse::AddressSpaces(vec![AddressSpaceInfo {
                name: "main".to_string(),
                addr_width_bits: 16,
                bank_context: Vec::new(),
            }]))
        }
        DebugCommand::ReadMemory { address, len, .. } => {
            let bytes = (0..len)
                .map(|offset| record.machine.read_memory(address.wrapping_add(offset)))
                .collect();
            Ok(DebugResponse::Memory(bytes))
        }
        DebugCommand::WriteMemory { address, bytes, .. } => {
            record.machine.load(address, &bytes);
            Ok(DebugResponse::Ack)
        }
        DebugCommand::GetDisassembly { start, lines } => {
            let mut cursor = start.unwrap_or(record.machine.pc());
            let mut out = Vec::with_capacity(lines as usize);
            for _ in 0..lines {
                let raw: Vec<u8> = (0..4u16)
                    .map(|offset| record.machine.read_memory(cursor.wrapping_add(offset)))
                    .collect();
                let (text, consumed) = z80::disassemble::disassemble(&raw);
                let consumed = consumed.clamp(1, 4);
                out.push(DisassemblyLine {
                    address: cursor,
                    bytes: raw[..consumed].to_vec(),
                    text,
                });
                cursor = cursor.wrapping_add(consumed as u16);
            }
            Ok(DebugResponse::Disassembly(out))
        }
        DebugCommand::GetMediaSnapshot => Ok(DebugResponse::MediaSnapshot(MediaSnapshot {
            tape: None,
            disks: Vec::new(),
            cartridge: None,
        })),
        DebugCommand::GetChipSnapshots => {
            Ok(DebugResponse::ChipSnapshots(Vec::<ChipSnapshot>::new()))
        }
        DebugCommand::GetMachineMetadata => Ok(DebugResponse::MachineMetadata(MachineMetadata {
            machine_id: "zx81".to_string(),
            model: Some("zx81".to_string()),
            region: Some(record.region.clone()),
            notes: vec!["external machine plugin v1".to_string()],
            bitmap_palette: bitmap_palette(),
            bitmap_defaults: Some(bitmap_defaults()),
        })),
    }
}

fn handle_instance_call(
    instance: MachineInstanceHandle,
    request: MachineInstanceCallRequest,
) -> Result<MachineInstanceCallResponse, String> {
    let mut state = state()
        .lock()
        .map_err(|_| "plugin state mutex poisoned".to_string())?;
    let record = state.record_mut(instance)?;
    let response = handle_machine_command(record, request.command)?;
    Ok(MachineInstanceCallResponse { response })
}

unsafe extern "C" fn plugin_free_buffer_v1(buffer: RcBuffer) {
    // SAFETY: caller guarantees this buffer originated from RcBuffer::from_vec.
    let _ = unsafe { buffer.into_vec() };
}

unsafe extern "C" fn plugin_list_profiles_v1(req: RcSlice) -> RcCallResult {
    plugin_api::guard::guarded_call("list_profiles", || unsafe {
        plugin_list_profiles_inner(req)
    })
}

unsafe fn plugin_list_profiles_inner(req: RcSlice) -> RcCallResult {
    let request: plugin_api::ListProfilesRequest = match decode_request(req) {
        Ok(req) => req,
        Err(e) => return e.into_call_result(),
    };
    let _ = request.include_experimental;

    let response = plugin_api::ListProfilesResponse {
        profiles: vec![machine_profile()],
    };
    encode_ok(&response)
}

unsafe extern "C" fn plugin_create_instance_v1(req: RcSlice) -> RcCallResult {
    plugin_api::guard::guarded_call("create_instance", || unsafe {
        plugin_create_instance_inner(req)
    })
}

unsafe fn plugin_create_instance_inner(req: RcSlice) -> RcCallResult {
    let request: MachineCreateRequest = match decode_request(req) {
        Ok(req) => req,
        Err(e) => return e.into_call_result(),
    };

    let response = match create_machine(request) {
        Ok(response) => response,
        Err(error) => return RcCallResult::error_with_message(status_from_error(&error), &error),
    };
    encode_ok(&response)
}

unsafe extern "C" fn plugin_destroy_instance_v1(instance: MachineInstanceHandle) -> RcStatus {
    plugin_api::guard::guarded(
        "destroy_instance",
        RcStatus::from_code(StatusCode::InternalError),
        || unsafe { plugin_destroy_instance_inner(instance) },
    )
}

unsafe fn plugin_destroy_instance_inner(instance: MachineInstanceHandle) -> RcStatus {
    let mut state = match state().lock() {
        Ok(state) => state,
        Err(_) => return RcStatus::from_code(StatusCode::InternalError),
    };

    if state.instances.remove(&instance).is_some() {
        RcStatus::ok()
    } else {
        RcStatus::from_code(StatusCode::NotFound)
    }
}

unsafe extern "C" fn plugin_instance_call_v1(
    instance: MachineInstanceHandle,
    req: RcSlice,
) -> RcCallResult {
    plugin_api::guard::guarded_call("instance_call", || unsafe {
        plugin_instance_call_inner(instance, req)
    })
}

unsafe fn plugin_instance_call_inner(
    instance: MachineInstanceHandle,
    req: RcSlice,
) -> RcCallResult {
    let request: MachineInstanceCallRequest = match decode_request(req) {
        Ok(req) => req,
        Err(e) => return e.into_call_result(),
    };

    let response = match handle_instance_call(instance, request) {
        Ok(response) => response,
        Err(error) => return RcCallResult::error_with_message(status_from_error(&error), &error),
    };
    encode_ok(&response)
}

/// Fast path for `MachinePluginVTableV1::step` -- see that field's own doc
/// comment in `plugin-api`. No `MachineCommand`/bincode round trip at all.
extern "C" fn plugin_step_v1(instance: MachineInstanceHandle) -> u32 {
    if plugin_api::guard::has_crashed() {
        // A crashed machine idles like a halted CPU; 0 would spin the host.
        return 4;
    }
    plugin_api::guard::guarded("step", 4, || plugin_step_inner(instance))
}

fn plugin_step_inner(instance: MachineInstanceHandle) -> u32 {
    let Ok(mut state) = state().lock() else {
        return 0;
    };
    match state.instances.get_mut(&instance) {
        Some(record) => record.machine.step(),
        None => 0,
    }
}

/// Fast path for `MachinePluginVTableV1::halted`.
extern "C" fn plugin_halted_v1(instance: MachineInstanceHandle) -> u8 {
    if plugin_api::guard::has_crashed() {
        return 1;
    }
    plugin_api::guard::guarded("halted", 1, || plugin_halted_inner(instance))
}

fn plugin_halted_inner(instance: MachineInstanceHandle) -> u8 {
    let Ok(state) = state().lock() else {
        return 1;
    };
    match state.instances.get(&instance) {
        Some(record) => u8::from(record.machine.halted()),
        None => 1,
    }
}

/// Fast path for `MachinePluginVTableV1::pc`.
extern "C" fn plugin_pc_v1(instance: MachineInstanceHandle) -> u16 {
    plugin_api::guard::guarded("pc", 0, || plugin_pc_inner(instance))
}

fn plugin_pc_inner(instance: MachineInstanceHandle) -> u16 {
    let Ok(state) = state().lock() else {
        return 0;
    };
    match state.instances.get(&instance) {
        Some(record) => record.machine.pc(),
        None => 0,
    }
}

/// # Safety
///
/// Returns immutable descriptor storage owned by this plugin.
pub unsafe extern "C" fn entry_v1() -> *const PluginDescriptorV1 {
    &PLUGIN_DESCRIPTOR_V1.0
}

#[cfg(test)]
mod tests {
    #[test]
    fn key_names_list_everything_the_key_event_path_accepts() {
        let names = key_names();
        let listed: Vec<&str> = names.iter().map(|k| k.name.as_str()).collect();
        for expected in ["KeyA", "Digit0", "Space", "Enter", "ShiftLeft", "ArrowUp"] {
            assert!(
                listed.contains(&expected),
                "{expected} missing from {listed:?}"
            );
        }
        // Unique (ignoring case/underscores), and each entry matches its own name.
        let mut seen = std::collections::HashSet::new();
        for key in &names {
            assert!(
                seen.insert(plugin_api::normalize_key_name(&key.name)),
                "{}",
                key.name
            );
            assert!(key.matches(&key.name));
        }
        // The free aliases work: `a` finds KeyA.
        assert!(names.iter().any(|k| k.name == "KeyA" && k.matches("a")));
    }

    #[test]
    fn the_dot_key_and_the_shifted_symbols_are_named_and_typable() {
        // The ZX81 has 40 keys; the `.` key was missing from the list for a long time.
        assert_eq!(zx81_key_from_name("Period"), Some(Zx81Key::Dot));
        // Shift plus a key, per the ROM's shifted key table (0x00A5): `+` is Shift+K, `*` Shift+B, `"` Shift+P, ...
        for (name, key) in [
            ("Plus", Zx81Key::K),
            ("Asterisk", Zx81Key::B),
            ("DoubleQuote", Zx81Key::P),
            ("Colon", Zx81Key::Z),
            ("Equals", Zx81Key::L),
            ("Comma", Zx81Key::Dot),
        ] {
            assert_eq!(zx81_combo_from_name(name), Some(key), "{name}");
        }
        assert_eq!(
            zx81_combo_from_name("Period"),
            None,
            "Period is a key, not a combo"
        );
        // Every combo and every typed character is a listed key, each character belongs to one key, and the
        // characters type what their names say.
        let names = key_names();
        for (name, _) in zx81_combo_table() {
            assert!(names.iter().any(|k| k.name == *name), "{name}");
        }
        let mut typed = std::collections::HashSet::new();
        for (name, chars) in ZX81_CHARS {
            assert!(names.iter().any(|k| k.name == *name), "{name}");
            assert!(typed.insert(*chars), "{chars:?} typed by two keys");
        }
        for symbol in "+-*/=()<>:;?,.\"$ ".chars() {
            assert!(
                typed.contains(symbol.to_string().as_str()),
                "{symbol:?} cannot be typed"
            );
        }
    }

    use super::*;

    fn payload(result: RcCallResult) -> Vec<u8> {
        assert_eq!(result.status.code, StatusCode::Ok as u32);
        // SAFETY: tests consume payload exactly once.
        unsafe { result.payload.into_vec() }
    }

    fn zx81_request() -> MachineCreateRequest {
        MachineCreateRequest {
            machine_id: "zx81".to_string(),
            region: Some("pal".to_string()),
            roms: vec![BinaryBlob {
                key: "rom".to_string(),
                bytes: vec![0; zx81::ROM_SIZE],
            }],
            options: Vec::new(),
        }
    }

    #[test]
    fn descriptor_is_machine_plugin_v1() {
        let ptr = unsafe { entry_v1() };
        assert!(!ptr.is_null());
        // SAFETY: entry function returns pointer to static descriptor.
        let descriptor = unsafe { &*ptr };
        assert_eq!(descriptor.abi_major, ABI_MAJOR_V1);
        assert_eq!(descriptor.abi_minor, ABI_MINOR_V1);
        assert_eq!(descriptor.kind, PluginKind::Machine);
    }

    #[test]
    fn list_profiles_returns_zx81_profile() {
        let req = plugin_api::ListProfilesRequest {
            include_experimental: false,
        };
        let req_bytes = plugin_api::encode_message(&req).expect("encode request");
        // SAFETY: ABI function consumes request bytes only during call.
        let result = unsafe { plugin_list_profiles_v1(RcSlice::from_bytes(&req_bytes)) };
        let bytes = payload(result);
        let response: plugin_api::ListProfilesResponse =
            plugin_api::decode_message(&bytes).expect("decode response");
        assert_eq!(response.profiles.len(), 1);
        assert_eq!(response.profiles[0].machine_id, "zx81");
    }

    #[test]
    fn create_instance_and_get_pc_round_trip() {
        let create_req = plugin_api::encode_message(&zx81_request()).expect("encode create req");
        // SAFETY: ABI function consumes request bytes only during call.
        let create_result = unsafe { plugin_create_instance_v1(RcSlice::from_bytes(&create_req)) };
        let create_payload = payload(create_result);
        let create: MachineCreateResponse =
            plugin_api::decode_message(&create_payload).expect("decode create response");

        let call_req = MachineInstanceCallRequest {
            command: MachineCommand::GetPc,
        };
        let call_req_bytes = plugin_api::encode_message(&call_req).expect("encode call request");
        // SAFETY: ABI function consumes request bytes only during call.
        let call_result = unsafe {
            plugin_instance_call_v1(create.instance, RcSlice::from_bytes(&call_req_bytes))
        };
        let call_payload = payload(call_result);
        let call: MachineInstanceCallResponse =
            plugin_api::decode_message(&call_payload).expect("decode call response");

        assert_eq!(call.response, MachineResponse::Pc(0));

        // SAFETY: instance handle came from create response above.
        let status = unsafe { plugin_destroy_instance_v1(create.instance) };
        assert!(status.is_ok());
    }

    /// A banked/paged poke (`WriteMemory` with a `space` set) must be a
    /// clear error, not a silent flat write -- ZX81 has no banked memory
    /// at all, so silently ignoring `space` (as an earlier version of this
    /// handler did) would poke the wrong place without telling the caller.
    #[test]
    fn write_memory_with_a_space_is_a_clear_error() {
        let create_req = plugin_api::encode_message(&zx81_request()).expect("encode create req");
        // SAFETY: ABI function consumes request bytes only during call.
        let create_result = unsafe { plugin_create_instance_v1(RcSlice::from_bytes(&create_req)) };
        let create: MachineCreateResponse =
            plugin_api::decode_message(&payload(create_result)).expect("decode create response");

        let write_req = MachineInstanceCallRequest {
            command: MachineCommand::WriteMemory {
                address: 0x4009,
                bytes: vec![0x42],
                space: Some("ram-bank:1".to_string()),
            },
        };
        let write_bytes = plugin_api::encode_message(&write_req).expect("encode write request");
        // SAFETY: ABI function consumes request bytes only during call.
        let write_result =
            unsafe { plugin_instance_call_v1(create.instance, RcSlice::from_bytes(&write_bytes)) };
        assert_ne!(write_result.status.code, StatusCode::Ok as u32);

        // SAFETY: instance handle came from create response above.
        let status = unsafe { plugin_destroy_instance_v1(create.instance) };
        assert!(status.is_ok());
    }
}
