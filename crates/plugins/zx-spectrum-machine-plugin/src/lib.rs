use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use machine::{AUDIO_SAMPLE_RATE, Machine, Region};
use plugin_api::{
    ABI_MAJOR_V1, ABI_MINOR_V1, AddressSpaceInfo, BinaryBlob, BitmapExplorerDefaults,
    BitmapPaletteColor, ChipSnapshot, CodecError, CpuFamily, DebugCommand, DebugResponse,
    DebugValue, DisassemblyLine, DiskStatus, HostMouseButton, MACHINE_PROFILE_CONTRACT_VERSION,
    MachineCapability, MachineCommand, MachineCreateRequest, MachineCreateResponse,
    MachineInstanceCallRequest, MachineInstanceCallResponse, MachineInstanceHandle,
    MachineMetadata, MachineOptionInfo, MachineOptionKind, MachinePluginVTableV1, MachineProfile,
    MachineResponse, MediaKind, MediaSnapshot, NamedValue, PluginDescriptorV1, PluginKind,
    REQUIRED_MACHINE_DEBUG_CAPABILITIES, RcBuffer, RcCallResult, RcSlice, RcStatus, RegisterField,
    RegisterSnapshot, StatusCode, TapeStatus,
};
use zx_spectrum::{
    FullerButton, JoystickType, KempstonButton, Model, MouseButton, SpeakerType, ZxSpectrum,
    ZxSpectrumKey,
};

const PLUGIN_ID: &[u8] = b"retroclawd.zx-spectrum.machine";

fn bitmap_palette() -> Vec<BitmapPaletteColor> {
    let base = [
        ("black", (0x00, 0x00, 0x00)),
        ("blue", (0x00, 0x00, 0xCD)),
        ("red", (0xCD, 0x00, 0x00)),
        ("magenta", (0xCD, 0x00, 0xCD)),
        ("green", (0x00, 0xCD, 0x00)),
        ("cyan", (0x00, 0xCD, 0xCD)),
        ("yellow", (0xCD, 0xCD, 0x00)),
        ("white", (0xCD, 0xCD, 0xCD)),
    ];
    base.into_iter()
        .flat_map(|(name, (r, g, b))| {
            [
                BitmapPaletteColor {
                    name: name.to_string(),
                    rgb: [r, g, b],
                },
                BitmapPaletteColor {
                    name: format!("bright {name}"),
                    rgb: [
                        if r == 0 { 0 } else { 0xFF },
                        if g == 0 { 0 } else { 0xFF },
                        if b == 0 { 0 } else { 0xFF },
                    ],
                },
            ]
        })
        .collect()
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpectrumKind {
    Spectrum48,
    Spectrum128,
    Spectrum2A3,
}

impl SpectrumKind {
    fn all() -> [Self; 3] {
        [Self::Spectrum48, Self::Spectrum128, Self::Spectrum2A3]
    }

    fn from_machine_id(machine_id: &str) -> Option<Self> {
        if machine_id.eq_ignore_ascii_case("zx-spectrum") {
            Some(Self::Spectrum48)
        } else if machine_id.eq_ignore_ascii_case("zx-spectrum-128") {
            Some(Self::Spectrum128)
        } else if machine_id.eq_ignore_ascii_case("zx-spectrum-2a3") {
            Some(Self::Spectrum2A3)
        } else {
            None
        }
    }

    fn machine_id(self) -> &'static str {
        match self {
            Self::Spectrum48 => "zx-spectrum",
            Self::Spectrum128 => "zx-spectrum-128",
            Self::Spectrum2A3 => "zx-spectrum-2a3",
        }
    }

    fn model(self) -> Model {
        match self {
            Self::Spectrum48 => Model::Spectrum48,
            Self::Spectrum128 => Model::Spectrum128,
            Self::Spectrum2A3 => Model::Spectrum2A3,
        }
    }

    fn rom_size(self) -> usize {
        match self {
            Self::Spectrum48 => zx_spectrum::ROM_SIZE,
            Self::Spectrum128 => zx_spectrum::ROM_SIZE_128K,
            Self::Spectrum2A3 => zx_spectrum::ROM_SIZE_2A3,
        }
    }
}

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
    machine: ZxSpectrum,
    kind: SpectrumKind,
    region: String,
    paused: bool,
    breakpoints: Vec<u16>,
    /// Drive A and drive B.
    disk_attached: [bool; 2],
    disk_image_id: [Option<String>; 2],
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

fn machine_profiles() -> Vec<MachineProfile> {
    SpectrumKind::all()
        .into_iter()
        .map(machine_profile)
        .collect()
}

fn machine_profile(kind: SpectrumKind) -> MachineProfile {
    MachineProfile {
        machine_id: kind.machine_id().to_string(),
        display_name: kind.machine_id().to_string(),
        cpu_family: CpuFamily::Z80,
        default_region: Some("pal".to_string()),
        contract_version: MACHINE_PROFILE_CONTRACT_VERSION,
        debug_capabilities: REQUIRED_MACHINE_DEBUG_CAPABILITIES.to_vec(),
        capabilities: vec![
            MachineCapability::Video,
            MachineCapability::Audio,
            MachineCapability::Tape,
            MachineCapability::SaveState,
            MachineCapability::Snapshot,
            MachineCapability::Debug,
        ],
        media_kinds: vec![
            MediaKind::Snapshot,
            MediaKind::Tape,
            MediaKind::Program,
            MediaKind::Disk,
        ],
        options: zx_spectrum_options(kind),
        key_names: key_names(),
    }
}

fn zx_spectrum_options(kind: SpectrumKind) -> Vec<MachineOptionInfo> {
    let mut options = vec![
        MachineOptionInfo {
            key: "rom".to_string(),
            flag: "rom".to_string(),
            short: None,
            kind: MachineOptionKind::File,
            default: None,
            required: true,
            description: "ROM image (16K for 48K, 32K for 128K, 64K for +2A/+3)".to_string(),
        },
        MachineOptionInfo {
            key: "speaker_type".to_string(),
            flag: "speaker-type".to_string(),
            short: None,
            kind: MachineOptionKind::Enum {
                choices: vec![
                    "unfiltered".to_string(),
                    "beeper".to_string(),
                    "tv-speaker".to_string(),
                ],
            },
            default: Some(DebugValue::Text("unfiltered".to_string())),
            required: false,
            description: "Post-mix speaker frequency-rolloff approximation".to_string(),
        },
        MachineOptionInfo {
            key: "joystick_type".to_string(),
            flag: "joystick".to_string(),
            short: None,
            kind: MachineOptionKind::Enum {
                choices: vec![
                    "kempston".to_string(),
                    "sinclair1".to_string(),
                    "sinclair2".to_string(),
                    "cursor".to_string(),
                    "fuller".to_string(),
                ],
            },
            default: Some(DebugValue::Text("kempston".to_string())),
            required: false,
            description: "Joystick convention arrow-keys+Space drive".to_string(),
        },
        MachineOptionInfo {
            key: "fast_tape_load".to_string(),
            flag: "fast-tape-load".to_string(),
            short: None,
            kind: MachineOptionKind::Bool,
            default: Some(DebugValue::Bool(false)),
            required: false,
            description: "Trap the ROM LD-BYTES routine for near-instant tape loading".to_string(),
        },
        MachineOptionInfo {
            key: "tape_start".to_string(),
            flag: "tape-start".to_string(),
            short: None,
            kind: MachineOptionKind::Enum {
                choices: vec!["rom-load".to_string(), "any-read".to_string()],
            },
            default: Some(DebugValue::Text("rom-load".to_string())),
            required: false,
            description: "When a --tape starts playing: rom-load = when the ROM's LOAD asks for it \
                (like pressing Play); any-read = at the first read of port 0xFE, i.e. from power-on \
                (for a custom loader started from a snapshot)"
                .to_string(),
        },
    ];

    if matches!(kind, SpectrumKind::Spectrum128 | SpectrumKind::Spectrum2A3) {
        options.push(MachineOptionInfo {
            key: "turbosound".to_string(),
            flag: "turbosound".to_string(),
            short: None,
            kind: MachineOptionKind::Bool,
            default: Some(DebugValue::Bool(false)),
            required: false,
            description: "Second AY-3-8910 selected via the real Turbosound chip-select protocol"
                .to_string(),
        });
    }

    options
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

fn region_name(region: Region) -> &'static str {
    match region {
        Region::Pal => "pal",
        Region::Ntsc => "ntsc",
        Region::Japan => "japan",
    }
}

fn blob<'a>(roms: &'a [BinaryBlob], key: &str) -> Option<&'a [u8]> {
    roms.iter()
        .find(|blob| blob.key.eq_ignore_ascii_case(key))
        .map(|blob| blob.bytes.as_slice())
}

fn option_value<'a>(options: &'a [NamedValue], key: &str) -> Option<&'a DebugValue> {
    options
        .iter()
        .find(|item| item.key.eq_ignore_ascii_case(key))
        .map(|item| &item.value)
}

fn option_u32(options: &[NamedValue], key: &str) -> Result<Option<u32>, String> {
    let Some(value) = option_value(options, key) else {
        return Ok(None);
    };

    match value {
        DebugValue::U64(v) => u32::try_from(*v)
            .map(Some)
            .map_err(|_| format!("option \"{key}\" out of range for u32")),
        _ => Err(format!("option \"{key}\" must be an unsigned integer")),
    }
}

fn option_bool(options: &[NamedValue], key: &str) -> Result<Option<bool>, String> {
    let Some(value) = option_value(options, key) else {
        return Ok(None);
    };

    match value {
        DebugValue::Bool(v) => Ok(Some(*v)),
        _ => Err(format!("option \"{key}\" must be a bool")),
    }
}

fn option_text(options: &[NamedValue], key: &str) -> Result<Option<String>, String> {
    let Some(value) = option_value(options, key) else {
        return Ok(None);
    };

    match value {
        DebugValue::Text(v) => Ok(Some(v.clone())),
        _ => Err(format!("option \"{key}\" must be text")),
    }
}

fn parse_speaker_type(value: &str) -> Result<SpeakerType, String> {
    match value {
        "unfiltered" => Ok(SpeakerType::Unfiltered),
        "beeper" => Ok(SpeakerType::Beeper),
        "tv-speaker" => Ok(SpeakerType::TvSpeaker),
        other => Err(format!("unsupported speaker type \"{other}\"")),
    }
}

fn parse_joystick_type(value: &str) -> Result<JoystickType, String> {
    match value {
        "kempston" => Ok(JoystickType::Kempston),
        "sinclair1" => Ok(JoystickType::Sinclair1),
        "sinclair2" => Ok(JoystickType::Sinclair2),
        "cursor" => Ok(JoystickType::Cursor),
        "fuller" => Ok(JoystickType::Fuller),
        other => Err(format!("unsupported joystick type \"{other}\"")),
    }
}

fn create_machine(req: MachineCreateRequest) -> Result<MachineCreateResponse, String> {
    let kind = SpectrumKind::from_machine_id(&req.machine_id)
        .ok_or_else(|| format!("unsupported machine id \"{}\"", req.machine_id))?;
    let rom =
        blob(&req.roms, "rom").ok_or_else(|| "missing required binary blob: rom".to_string())?;
    if rom.len() != kind.rom_size() {
        return Err(format!(
            "rom blob has wrong size: expected {} bytes, got {}",
            kind.rom_size(),
            rom.len()
        ));
    }

    let region = parse_region(req.region.as_deref())?;
    let mut machine = ZxSpectrum::new(rom.to_vec(), region, kind.model());

    if let Some(enabled) = option_bool(&req.options, "border_enabled")? {
        machine.set_border(enabled);
    }
    if let Some(enabled) = option_bool(&req.options, "fast_tape_load")? {
        machine.set_fast_tape_load(enabled);
    }
    if let Some(start) = option_text(&req.options, "tape_start")? {
        match start.as_str() {
            "rom-load" => machine.set_tape_start_on_any_read(false),
            "any-read" => machine.set_tape_start_on_any_read(true),
            other => {
                return Err(format!(
                    "tape_start must be rom-load or any-read, not \"{other}\""
                ));
            }
        }
    }
    if let Some(speaker) = option_text(&req.options, "speaker_type")? {
        machine.set_speaker_type(parse_speaker_type(&speaker)?);
    }
    if let Some(joystick) = option_text(&req.options, "joystick_type")? {
        machine.set_joystick_type(parse_joystick_type(&joystick)?);
    }
    if let Some(enabled) = option_bool(&req.options, "turbosound")? {
        machine.set_turbosound(enabled);
    }

    let mut state = state()
        .lock()
        .map_err(|_| "plugin state mutex poisoned".to_string())?;
    let instance = state.allocate_instance();
    state.instances.insert(
        instance,
        InstanceRecord {
            machine,
            kind,
            region: region_name(region).to_string(),
            paused: false,
            breakpoints: Vec::new(),
            disk_attached: [false; 2],
            disk_image_id: [None, None],
        },
    );

    Ok(MachineCreateResponse {
        instance,
        metadata: MachineMetadata {
            machine_id: kind.machine_id().to_string(),
            model: Some(kind.machine_id().to_string()),
            region: Some(region_name(region).to_string()),
            notes: vec!["external machine plugin v1".to_string()],
            bitmap_palette: bitmap_palette(),
            bitmap_defaults: Some(bitmap_defaults()),
        },
    })
}

fn media_extension(kind: MediaKind, name: Option<&str>) -> String {
    if let Some(filename) = name
        && let Some(ext) = filename.rsplit('.').next()
    {
        return ext.to_ascii_lowercase();
    }

    match kind {
        MediaKind::Snapshot => "z80".to_string(),
        MediaKind::Tape => "tap".to_string(),
        MediaKind::Disk => "dsk".to_string(),
        MediaKind::Program => "bin".to_string(),
        MediaKind::Cartridge => "cart".to_string(),
        MediaKind::RomSet => "rom".to_string(),
        MediaKind::Other(other) => other,
    }
}

fn save_snapshot_bytes(record: &InstanceRecord, extension: &str) -> Result<Vec<u8>, String> {
    match extension {
        "z80" => z80_snapshot::write(&record.machine.save_snapshot()).map_err(|e| e.to_string()),
        "sna" => {
            let snapshot = record.machine.save_sna_snapshot()?;
            sna_snapshot::write(&snapshot).map_err(|e| e.to_string())
        }
        "szx" => {
            let snapshot = record.machine.save_szx_snapshot();
            Ok(szx_snapshot::write(&snapshot))
        }
        other => Err(format!(
            "unsupported snapshot save extension for zx-spectrum plugin: .{other} (expected .z80, .sna, or .szx)"
        )),
    }
}

fn load_snapshot_bytes(
    record: &mut InstanceRecord,
    extension: &str,
    bytes: &[u8],
) -> Result<(), String> {
    match extension {
        "z80" => {
            let snapshot = z80_snapshot::parse(bytes).map_err(|e| e.to_string())?;
            record.machine.load_snapshot(snapshot)
        }
        "sna" => {
            let snapshot = sna_snapshot::parse(bytes).map_err(|e| e.to_string())?;
            record.machine.load_sna_snapshot(snapshot)
        }
        "szx" => {
            let snapshot = szx_snapshot::parse(bytes).map_err(|e| e.to_string())?;
            record.machine.load_szx_snapshot(snapshot)
        }
        other => Err(format!(
            "unsupported snapshot extension for zx-spectrum plugin: .{other} (expected .z80, .sna, or .szx)"
        )),
    }
}

fn attach_media(
    slot: plugin_api::MediaSlot,
    record: &mut InstanceRecord,
    kind: MediaKind,
    name: Option<String>,
    bytes: Vec<u8>,
    options: Vec<NamedValue>,
) -> Result<(), String> {
    match kind {
        MediaKind::Snapshot => {
            let extension = media_extension(MediaKind::Snapshot, name.as_deref());
            load_snapshot_bytes(record, &extension, &bytes)
        }
        MediaKind::Tape => {
            let extension = media_extension(MediaKind::Tape, name.as_deref());
            let tape = match extension.as_str() {
                "tap" => zx_tape::parse_tap(&bytes).map_err(|e| e.to_string())?,
                "tzx" | "cdt" => zx_tape::parse_tzx(&bytes).map_err(|e| e.to_string())?,
                "pzx" => zx_tape::parse_pzx(&bytes).map_err(|e| e.to_string())?,
                "csw" => zx_tape::parse_csw(&bytes).map_err(|e| e.to_string())?,
                "voc" => zx_tape::parse_voc(&bytes).map_err(|e| e.to_string())?,
                "wav" => zx_tape::parse_wav(&bytes).map_err(|e| e.to_string())?,
                other => {
                    return Err(format!(
                        "unsupported tape extension for zx-spectrum plugin: .{other} (expected .tap, .tzx, .cdt, .pzx, .csw, .voc or .wav)"
                    ));
                }
            };
            let speed = option_u32(&options, "tape_speed")
                .or_else(|_| option_u32(&options, "speed_multiplier"))?
                .unwrap_or(1);
            record.machine.load_tape(tape);
            record.machine.set_tape_speed(speed);
            Ok(())
        }
        MediaKind::Disk => {
            let extension = media_extension(MediaKind::Disk, name.as_deref());
            let disk = match extension.as_str() {
                "dsk" | "edsk" => dsk::parse(&bytes).map_err(|e| e.to_string())?,
                other => {
                    return Err(format!(
                        "unsupported disk extension for zx-spectrum plugin: .{other} (expected .dsk or .edsk)"
                    ));
                }
            };
            let drive = match slot {
                plugin_api::MediaSlot::Disk { drive } => usize::from(drive),
                _ => 0,
            };
            record.machine.load_disk_in(drive, disk)?;
            record.disk_attached[drive] = true;
            record.disk_image_id[drive] = name;
            Ok(())
        }
        MediaKind::Program => {
            let org = option_u32(&options, "org")?.unwrap_or(0x4000);
            let org = u16::try_from(org)
                .map_err(|_| format!("program org out of range for u16: {org}"))?;
            record.machine.load(org, &bytes);
            Ok(())
        }
        other => Err(format!(
            "unsupported media kind for zx-spectrum plugin: {:?}",
            other
        )),
    }
}

/// A drive number from the host: the +3 has the internal drive A (0) and an optional external drive B (1).
fn disk_drive(drive: u8) -> Result<usize, String> {
    if drive < 2 {
        Ok(usize::from(drive))
    } else {
        Err(format!("drive {drive} does not exist (drives 0 and 1)"))
    }
}

fn media_snapshot(record: &InstanceRecord) -> MediaSnapshot {
    let tape = record.machine.tape_playback_state().map(|s| TapeStatus {
        attached: true,
        playing: s.playing,
        finished: s.finished,
        speed_multiplier: s.speed_multiplier,
        position: Some(s.position as u64),
        total: Some(s.events as u64),
        motor_on: None,
        ear_bit: record.machine.tape_ear_bit(),
        cycles_until_next_edge: record.machine.tape_cycles_until_next_edge().map(u64::from),
        spin_down_cycles: None,
    });

    let disks = if matches!(record.kind, SpectrumKind::Spectrum2A3) {
        (0..2)
            .map(|drive| DiskStatus {
                attached: record.disk_attached[drive],
                image_id: record.disk_image_id[drive].clone(),
                drive: Some(drive as u8),
                head: None,
                track: None,
                sector: None,
                controller_phase: record.machine.overlay_fdc_phase(),
            })
            .collect()
    } else {
        Vec::new()
    };

    MediaSnapshot {
        tape,
        disks,
        cartridge: None,
    }
}

fn nv_text(key: &str, value: impl Into<String>) -> NamedValue {
    NamedValue {
        key: key.to_string(),
        value: DebugValue::Text(value.into()),
    }
}

fn nv_bool(key: &str, value: bool) -> NamedValue {
    NamedValue {
        key: key.to_string(),
        value: DebugValue::Bool(value),
    }
}

/// Machine-agnostic overlay chip snapshot (`chip_id: "overlay"`) consumed
/// by the on-screen `--show-overlay` GUI panel -- same generic field
/// vocabulary as `cpc-machine-plugin`'s own `generic_overlay_chip_snapshot`
/// (`memory_summary`/`fdc_phase`/`disk_motor`/`tape_motor` +
/// `tape_waveform` blob), letting the host render one uniform panel set
/// without per-machine branching. Fields are omitted (not defaulted to a
/// placeholder) when this model/config has nothing to report -- e.g. no
/// `fdc_phase`/`disk_motor` off `Model::Spectrum2A3`, no `tape_motor` with
/// no tape installed.
fn nv_u64(key: &str, value: u64) -> NamedValue {
    NamedValue {
        key: key.to_string(),
        value: DebugValue::U64(value),
    }
}

fn generic_overlay_chip_snapshot(record: &InstanceRecord) -> ChipSnapshot {
    let mut fields = Vec::new();
    if let Some(summary) = record.machine.overlay_memory_summary() {
        fields.push(nv_text("memory_summary", summary));
    }
    if let Some(phase) = record.machine.overlay_fdc_phase() {
        fields.push(nv_text("fdc_phase", phase));
    }
    if let Some(motor) = record.machine.overlay_disk_motor() {
        fields.push(nv_bool("disk_motor", motor));
    }
    if let Some(playback) = record.machine.tape_playback_state() {
        fields.push(nv_bool("tape_motor", playback.playing));
        fields.push(nv_u64("tape_position", playback.position as u64));
        fields.push(nv_u64("tape_total", playback.events as u64));
    }
    if let Some(edge_cycles) = record.machine.tape_cycles_until_next_edge() {
        fields.push(nv_u64("tape_edge_cycles", u64::from(edge_cycles)));
    }
    let pulse_width_bytes = record
        .machine
        .overlay_tape_pulse_widths()
        .iter()
        .flat_map(|width| width.to_le_bytes())
        .collect();
    ChipSnapshot {
        chip_id: "overlay".to_string(),
        fields,
        blobs: vec![
            BinaryBlob {
                key: "tape_waveform".to_string(),
                bytes: record.machine.overlay_tape_waveform(),
            },
            BinaryBlob {
                key: "tape_pulse_widths".to_string(),
                bytes: pulse_width_bytes,
            },
        ],
    }
}

fn spectrum_combo_table() -> &'static [(&'static str, (ZxSpectrumKey, ZxSpectrumKey))] {
    use ZxSpectrumKey::*;
    &[
        ("Exclamation", (SymShift, Digit1)),
        ("At", (SymShift, Digit2)),
        ("Hash", (SymShift, Digit3)),
        ("Dollar", (SymShift, Digit4)),
        ("Percent", (SymShift, Digit5)),
        ("Ampersand", (SymShift, Digit6)),
        ("Apostrophe", (SymShift, Digit7)),
        ("LeftParen", (SymShift, Digit8)),
        ("RightParen", (SymShift, Digit9)),
        ("Underscore", (SymShift, Digit0)),
        ("LessEqual", (SymShift, Q)),
        ("GreaterEqual", (SymShift, W)),
        ("LessThan", (SymShift, E)),
        ("GreaterThan", (SymShift, R)),
        ("Question", (SymShift, T)),
        ("LeftBracket", (SymShift, Y)),
        ("RightBracket", (SymShift, U)),
        ("DoubleQuote", (SymShift, I)),
        ("Quote", (SymShift, I)),
        ("Semicolon", (SymShift, O)),
        ("Colon", (SymShift, Z)),
        ("Minus", (SymShift, J)),
        ("Plus", (SymShift, K)),
        ("Equals", (SymShift, L)),
        ("Slash", (SymShift, V)),
        ("Asterisk", (SymShift, B)),
        ("Comma", (SymShift, N)),
        ("Period", (SymShift, M)),
        ("Delete", (CapsShift, Digit0)),
    ]
}

fn spectrum_combo_from_name(name: &str) -> Option<(ZxSpectrumKey, ZxSpectrumKey)> {
    spectrum_combo_table()
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, value)| *value)
}

/// Every key name this machine's `KeyEvent` accepts -- built from the same
/// tables the event handler looks names up in, so the list cannot drift.
fn key_names() -> Vec<plugin_api::MachineKeyInfo> {
    let mut infos = plugin_api::key_infos(
        spectrum_key_table()
            .iter()
            .map(|(name, _)| *name)
            .chain(spectrum_combo_table().iter().map(|(name, _)| *name))
            .chain(JOYSTICK_KEYS.iter().map(|(name, _)| *name)),
    );
    plugin_api::set_key_chars(&mut infos, SPECTRUM_CHARS);
    infos
}

/// The ASCII character each key types on a real Spectrum (its Symbol Shift
/// legends), for the host's symbolic keyboard layout. `LessEqual`/
/// `GreaterEqual`/`Quote` have no single character of their own.
const SPECTRUM_CHARS: &[(&str, &str)] = &[
    ("Space", " "),
    ("Exclamation", "!"),
    ("At", "@"),
    ("Hash", "#"),
    ("Dollar", "$"),
    ("Percent", "%"),
    ("Ampersand", "&"),
    ("Apostrophe", "'"),
    ("LeftParen", "("),
    ("RightParen", ")"),
    ("Underscore", "_"),
    ("LessThan", "<"),
    ("GreaterThan", ">"),
    ("Question", "?"),
    ("LeftBracket", "["),
    ("RightBracket", "]"),
    ("DoubleQuote", "\""),
    ("Semicolon", ";"),
    ("Colon", ":"),
    ("Minus", "-"),
    ("Plus", "+"),
    ("Equals", "="),
    ("Slash", "/"),
    ("Asterisk", "*"),
    ("Comma", ","),
    ("Period", "."),
];

/// Joystick-only key names (`[pads]` and `[keys]` can bind to these): each
/// drives the joystick the `joystick_type` option selected (Kempston, Fuller,
/// or the Sinclair/Cursor keyboard-wired ones) without pressing the matching
/// cursor key as the arrow keys do. The second field is the arrow-key/Space
/// name the handler's joystick tables are written in.
const JOYSTICK_KEYS: [(&str, &str); 5] = [
    ("Joy1Up", "ArrowUp"),
    ("Joy1Down", "ArrowDown"),
    ("Joy1Left", "ArrowLeft"),
    ("Joy1Right", "ArrowRight"),
    ("Joy1Fire1", "Space"),
];

fn joystick_key_alias(name: &str) -> Option<&'static str> {
    JOYSTICK_KEYS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, alias)| *alias)
}

fn spectrum_key_table() -> &'static [(&'static str, ZxSpectrumKey)] {
    use ZxSpectrumKey::*;
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
        ("CapsShift", CapsShift),
        ("ShiftLeft", CapsShift),
        ("ShiftRight", CapsShift),
        ("shift", CapsShift),
        ("SymShift", SymShift),
        ("SymbolShift", SymShift),
        ("symbol_shift", SymShift),
        ("Digit9", Digit9),
        ("Space", Space),
        ("Enter", Enter),
        ("ArrowLeft", Digit5),
        ("ArrowDown", Digit6),
        ("ArrowUp", Digit7),
        ("ArrowRight", Digit8),
    ]
}

fn spectrum_key_from_name(name: &str) -> Option<ZxSpectrumKey> {
    spectrum_key_table()
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, value)| *value)
}

fn kempston_button_from_name(name: &str) -> Option<KempstonButton> {
    use KempstonButton::*;

    Some(match name {
        "ArrowRight" => Right,
        "ArrowLeft" => Left,
        "ArrowDown" => Down,
        "ArrowUp" => Up,
        "Space" => Fire,
        _ => return None,
    })
}

fn fuller_button_from_name(name: &str) -> Option<FullerButton> {
    use FullerButton::*;

    Some(match name {
        "ArrowRight" => Right,
        "ArrowLeft" => Left,
        "ArrowDown" => Down,
        "ArrowUp" => Up,
        "Space" => Fire,
        _ => return None,
    })
}

fn keyboard_joystick_key(joystick: JoystickType, key_name: &str) -> Option<ZxSpectrumKey> {
    use ZxSpectrumKey::*;

    Some(match (joystick, key_name) {
        (JoystickType::Sinclair1, "ArrowLeft") => Digit1,
        (JoystickType::Sinclair1, "ArrowRight") => Digit2,
        (JoystickType::Sinclair1, "ArrowDown") => Digit3,
        (JoystickType::Sinclair1, "ArrowUp") => Digit4,
        (JoystickType::Sinclair1, "Space") => Digit5,

        (JoystickType::Sinclair2, "ArrowLeft") => Digit6,
        (JoystickType::Sinclair2, "ArrowRight") => Digit7,
        (JoystickType::Sinclair2, "ArrowDown") => Digit8,
        (JoystickType::Sinclair2, "ArrowUp") => Digit9,
        (JoystickType::Sinclair2, "Space") => Digit0,

        (JoystickType::Cursor, "ArrowLeft") => Digit5,
        (JoystickType::Cursor, "ArrowDown") => Digit6,
        (JoystickType::Cursor, "ArrowUp") => Digit7,
        (JoystickType::Cursor, "ArrowRight") => Digit8,
        (JoystickType::Cursor, "Space") => Digit0,

        (JoystickType::Kempston, _) | (JoystickType::Fuller, _) => return None,
        _ => return None,
    })
}

fn mouse_button_from_host(button: HostMouseButton) -> Option<MouseButton> {
    match button {
        HostMouseButton::Left => Some(MouseButton::Left),
        HostMouseButton::Right => Some(MouseButton::Right),
        HostMouseButton::Middle => Some(MouseButton::Middle),
        HostMouseButton::Other(_) => None,
    }
}

fn apply_key_event(record: &mut InstanceRecord, key: &str, pressed: bool) {
    if let Some((modifier, base)) = spectrum_combo_from_name(key) {
        for emulated_key in [modifier, base] {
            if pressed {
                record.machine.press_key(emulated_key);
            } else {
                record.machine.release_key(emulated_key);
            }
        }
    } else if let Some(key) = spectrum_key_from_name(key) {
        if pressed {
            record.machine.press_key(key);
        } else {
            record.machine.release_key(key);
        }
    }

    // The arrow keys and Space double as the joystick; `Joy1*` names reach
    // the same joystick tables without touching the keyboard.
    let key = joystick_key_alias(key).unwrap_or(key);
    match record.machine.joystick_type() {
        JoystickType::Kempston => {
            if let Some(button) = kempston_button_from_name(key) {
                if pressed {
                    record.machine.press_kempston(button);
                } else {
                    record.machine.release_kempston(button);
                }
            }
        }
        joystick @ (JoystickType::Sinclair1 | JoystickType::Sinclair2 | JoystickType::Cursor) => {
            if let Some(key) = keyboard_joystick_key(joystick, key) {
                if pressed {
                    record.machine.press_key(key);
                } else {
                    record.machine.release_key(key);
                }
            }
        }
        JoystickType::Fuller => {
            if let Some(button) = fuller_button_from_name(key) {
                if pressed {
                    record.machine.press_fuller(button);
                } else {
                    record.machine.release_fuller(button);
                }
            }
        }
    }
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
    if value <= u8::MAX as u64 {
        8
    } else if value <= u16::MAX as u64 {
        16
    } else if value <= u32::MAX as u64 {
        32
    } else {
        64
    }
}

fn ram_bank_from_space(space: &str) -> Result<u8, String> {
    let value = space
        .strip_prefix("ram-bank:")
        .ok_or_else(|| format!("unsupported memory space {space:?}"))?
        .parse::<u8>()
        .map_err(|_| format!("invalid RAM bank memory space {space:?}"))?;
    if value >= 8 {
        return Err(format!("RAM bank out of range: {value}"));
    }
    Ok(value)
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
        MachineCommand::WriteMemory {
            address,
            bytes,
            space,
        } => {
            let written = match space {
                None => record.machine.load(address, &bytes),
                Some(space) => {
                    let bank = ram_bank_from_space(&space)?;
                    record.machine.write_ram_bank(bank, address, &bytes)
                }
            };
            Ok(MachineResponse::BytesWritten(written as u32))
        }
        MachineCommand::LoadBytes { address, bytes } => {
            let written = record.machine.load(address, &bytes);
            Ok(MachineResponse::BytesWritten(written as u32))
        }
        MachineCommand::SetPc { address } => {
            record.machine.set_pc(address);
            Ok(MachineResponse::Ack)
        }
        MachineCommand::SaveState => Ok(MachineResponse::StateBlob(record.machine.save_state())),
        MachineCommand::SaveSnapshot { extension } => {
            let extension = extension.to_ascii_lowercase();
            Ok(MachineResponse::SnapshotBlob(save_snapshot_bytes(
                record, &extension,
            )?))
        }
        MachineCommand::LoadState { bytes } => {
            record.machine.load_state(&bytes)?;
            Ok(MachineResponse::Ack)
        }
        MachineCommand::AttachMedia {
            slot,
            kind,
            name,
            bytes,
            options,
        } => {
            attach_media(slot, record, kind, name, bytes, options)?;
            Ok(MachineResponse::Ack)
        }
        MachineCommand::DetachMedia { slot } => match slot {
            plugin_api::MediaSlot::Tape => {
                record.machine.eject_tape();
                Ok(MachineResponse::Ack)
            }
            plugin_api::MediaSlot::Disk { drive } => {
                let drive = disk_drive(drive)?;
                record.machine.eject_disk_from(drive);
                record.disk_attached[drive] = false;
                record.disk_image_id[drive] = None;
                Ok(MachineResponse::Ack)
            }
            other => Err(format!(
                "nothing to eject from slot {other:?} on the zx-spectrum plugin (tape or a disk drive)"
            )),
        },
        MachineCommand::ExportDisk {
            drive,
            only_if_modified,
        } => {
            let drive = disk_drive(drive)?;
            let image = if only_if_modified {
                record.machine.take_modified_disk_image_of(drive)
            } else {
                record.machine.disk_in(drive).map(dsk::Disk::to_bytes)
            };
            Ok(MachineResponse::DiskImage(image))
        }
        MachineCommand::MarkDiskSaved { drive } => {
            record.machine.mark_disk_saved_in(disk_drive(drive)?);
            Ok(MachineResponse::Ack)
        }
        MachineCommand::ExportTape { .. } => {
            Err("unsupported: the zx-spectrum plugin does not record tape output".to_string())
        }
        MachineCommand::Freeze
        | MachineCommand::ExportCartridge
        | MachineCommand::ExportPrinter => {
            Err("unsupported: the zx-spectrum has no freezer or writable cartridge".to_string())
        }
        MachineCommand::GetMediaSnapshot => {
            Ok(MachineResponse::MediaSnapshot(media_snapshot(record)))
        }
        MachineCommand::SetBorder { enabled } => {
            record.machine.set_border(enabled);
            Ok(MachineResponse::Ack)
        }
        MachineCommand::KeyEvent { key, pressed } => {
            apply_key_event(record, &key, pressed);
            Ok(MachineResponse::Ack)
        }
        MachineCommand::MouseMotion { dx, dy } => {
            record.machine.move_mouse(dx, dy);
            Ok(MachineResponse::Ack)
        }
        MachineCommand::MouseButton { button, pressed } => {
            if let Some(button) = mouse_button_from_host(button) {
                if pressed {
                    record.machine.press_mouse_button(button);
                } else {
                    record.machine.release_mouse_button(button);
                }
            }
            Ok(MachineResponse::Ack)
        }
        MachineCommand::MouseWheel { delta } => {
            record.machine.scroll_mouse(delta);
            Ok(MachineResponse::Ack)
        }
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
            let (width, height, rgb) = record.machine.render_frame().ok_or_else(|| {
                format!("render_frame unavailable for {}", record.kind.machine_id())
            })?;
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
                name: "cpu".to_string(),
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
        DebugCommand::GetMediaSnapshot => Ok(DebugResponse::MediaSnapshot(media_snapshot(record))),
        DebugCommand::GetChipSnapshots => Ok(DebugResponse::ChipSnapshots(vec![
            generic_overlay_chip_snapshot(record),
        ])),
        DebugCommand::GetMachineMetadata => Ok(DebugResponse::MachineMetadata(MachineMetadata {
            machine_id: record.kind.machine_id().to_string(),
            model: Some(record.kind.machine_id().to_string()),
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
        profiles: machine_profiles(),
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

    match create_machine(request) {
        Ok(response) => encode_ok(&response),
        Err(error) => RcCallResult::error_with_message(status_from_error(&error), &error),
    }
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
        Ok(guard) => guard,
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

    match handle_instance_call(instance, request) {
        Ok(response) => encode_ok(&response),
        Err(error) => RcCallResult::error_with_message(status_from_error(&error), &error),
    }
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
    fn every_typed_character_belongs_to_a_listed_key() {
        let mut infos = key_names();
        assert!(plugin_api::set_key_chars(&mut infos, SPECTRUM_CHARS).is_empty());
        let typed = |name: &str| {
            infos
                .iter()
                .find(|k| k.name == name)
                .map(|k| k.chars.clone())
        };
        assert_eq!(typed("Exclamation").as_deref(), Some("!"));
        assert_eq!(typed("DoubleQuote").as_deref(), Some("\""));
        // No character is claimed by two keys.
        let all: String = infos.iter().map(|k| k.chars.as_str()).collect();
        let unique: std::collections::HashSet<char> = all.chars().collect();
        assert_eq!(all.chars().count(), unique.len(), "{all}");
    }

    #[test]
    fn key_names_list_everything_the_key_event_path_accepts() {
        let names = key_names();
        let listed: Vec<&str> = names.iter().map(|k| k.name.as_str()).collect();
        for expected in [
            "KeyA",
            "Digit5",
            "CapsShift",
            "SymShift",
            "Space",
            "Enter",
            "ArrowLeft",
            "Exclamation",
            "Delete",
            "Joy1Up",
            "Joy1Fire1",
        ] {
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

    use super::*;

    fn payload(result: RcCallResult) -> Vec<u8> {
        assert_eq!(result.status.code, StatusCode::Ok as u32);
        // SAFETY: tests consume payload exactly once.
        unsafe { result.payload.into_vec() }
    }

    fn create_request(machine_id: &str) -> MachineCreateRequest {
        let kind = SpectrumKind::from_machine_id(machine_id).expect("known machine id");
        MachineCreateRequest {
            machine_id: machine_id.to_string(),
            region: Some("pal".to_string()),
            roms: vec![BinaryBlob {
                key: "rom".to_string(),
                bytes: vec![0; kind.rom_size()],
            }],
            options: Vec::new(),
        }
    }

    #[test]
    fn the_plus3_has_two_drives_and_the_others_none() {
        let disk = |name: &str, drive: u8| MachineCommand::AttachMedia {
            slot: plugin_api::MediaSlot::Disk { drive },
            kind: MediaKind::Disk,
            name: Some(name.to_string()),
            bytes: dsk::Disk::new(1).to_bytes(),
            options: Vec::new(),
        };
        let export = |drive: u8| MachineCommand::ExportDisk {
            drive,
            only_if_modified: false,
        };
        let create = create_machine(create_request("zx-spectrum-2a3")).unwrap();
        let mut guard = state().lock().unwrap();
        let record = guard.instances.get_mut(&create.instance).unwrap();
        assert!(matches!(
            handle_machine_command(record, export(1)),
            Ok(MachineResponse::DiskImage(None))
        ));
        handle_machine_command(record, disk("a.dsk", 0)).unwrap();
        handle_machine_command(record, disk("b.dsk", 1)).unwrap();
        assert_eq!(record.disk_image_id[0].as_deref(), Some("a.dsk"));
        assert_eq!(record.disk_image_id[1].as_deref(), Some("b.dsk"));
        for drive in 0..2 {
            assert!(matches!(
                handle_machine_command(record, export(drive)),
                Ok(MachineResponse::DiskImage(Some(_)))
            ));
        }
        let snapshot = media_snapshot(record);
        assert_eq!(snapshot.disks.len(), 2);
        assert!(snapshot.disks.iter().all(|d| d.attached));
        // Ejecting B leaves A in; there is no drive C.
        handle_machine_command(
            record,
            MachineCommand::DetachMedia {
                slot: plugin_api::MediaSlot::Disk { drive: 1 },
            },
        )
        .unwrap();
        assert!(!record.disk_attached[1] && record.disk_attached[0]);
        assert!(matches!(
            handle_machine_command(record, export(1)),
            Ok(MachineResponse::DiskImage(None))
        ));
        assert!(handle_machine_command(record, disk("c.dsk", 2)).is_err());
        assert!(handle_machine_command(record, export(2)).is_err());
        assert!(
            handle_machine_command(record, MachineCommand::MarkDiskSaved { drive: 2 }).is_err()
        );
        assert!(matches!(
            handle_machine_command(record, MachineCommand::MarkDiskSaved { drive: 1 }),
            Ok(MachineResponse::Ack)
        ));
        guard.instances.remove(&create.instance);
        drop(guard);

        // A 48K has no controller to put a disk in, in either slot.
        let create = create_machine(create_request("zx-spectrum")).unwrap();
        let mut guard = state().lock().unwrap();
        let record = guard.instances.get_mut(&create.instance).unwrap();
        assert!(handle_machine_command(record, disk("a.dsk", 0)).is_err());
        assert!(handle_machine_command(record, disk("b.dsk", 1)).is_err());
        guard.instances.remove(&create.instance);
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
    fn joystick_key_names_reach_every_joystick_type_through_the_arrow_tables() {
        let alias = |name| joystick_key_alias(name).expect(name);
        assert_eq!(
            kempston_button_from_name(alias("Joy1Left")),
            Some(KempstonButton::Left)
        );
        assert_eq!(
            kempston_button_from_name(alias("Joy1Fire1")),
            Some(KempstonButton::Fire)
        );
        assert_eq!(
            fuller_button_from_name(alias("Joy1Up")),
            Some(FullerButton::Up)
        );
        assert_eq!(
            keyboard_joystick_key(JoystickType::Sinclair1, alias("Joy1Right")),
            Some(ZxSpectrumKey::Digit2)
        );
        assert_eq!(
            keyboard_joystick_key(JoystickType::Cursor, alias("Joy1Fire1")),
            Some(ZxSpectrumKey::Digit0)
        );
        // They are joystick-only: no keyboard key answers to these names.
        assert!(spectrum_key_from_name("Joy1Left").is_none());
        assert!(spectrum_combo_from_name("Joy1Left").is_none());
        assert!(joystick_key_alias("KeyA").is_none());
    }

    #[test]
    fn punctuation_names_map_to_symbol_shift_combinations() {
        assert_eq!(
            spectrum_combo_from_name("Semicolon"),
            Some((ZxSpectrumKey::SymShift, ZxSpectrumKey::O))
        );
        assert_eq!(
            spectrum_combo_from_name("Delete"),
            Some((ZxSpectrumKey::CapsShift, ZxSpectrumKey::Digit0))
        );
        assert_eq!(
            spectrum_key_from_name("symbol_shift"),
            Some(ZxSpectrumKey::SymShift)
        );
    }

    #[test]
    fn list_profiles_returns_all_spectrum_models() {
        let req = plugin_api::ListProfilesRequest {
            include_experimental: false,
        };
        let req_bytes = plugin_api::encode_message(&req).expect("encode request");
        // SAFETY: ABI function consumes request bytes only during call.
        let result = unsafe { plugin_list_profiles_v1(RcSlice::from_bytes(&req_bytes)) };
        let bytes = payload(result);
        let response: plugin_api::ListProfilesResponse =
            plugin_api::decode_message(&bytes).expect("decode response");

        let ids: Vec<String> = response
            .profiles
            .into_iter()
            .map(|p| p.machine_id)
            .collect();
        assert_eq!(
            ids,
            vec!["zx-spectrum", "zx-spectrum-128", "zx-spectrum-2a3"]
        );
    }

    #[test]
    fn create_instance_and_get_pc_round_trip() {
        let create_req =
            plugin_api::encode_message(&create_request("zx-spectrum")).expect("encode create req");
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

    #[test]
    fn set_controls_and_snapshot_save_round_trip() {
        let create_req =
            plugin_api::encode_message(&create_request("zx-spectrum")).expect("encode create req");
        // SAFETY: ABI function consumes request bytes only during call.
        let create_result = unsafe { plugin_create_instance_v1(RcSlice::from_bytes(&create_req)) };
        let create_payload = payload(create_result);
        let create: MachineCreateResponse =
            plugin_api::decode_message(&create_payload).expect("decode create response");

        {
            let command = MachineCommand::SetBorder { enabled: false };
            let req = plugin_api::encode_message(&MachineInstanceCallRequest { command })
                .expect("encode command request");
            // SAFETY: ABI function consumes request bytes only during call.
            let result =
                unsafe { plugin_instance_call_v1(create.instance, RcSlice::from_bytes(&req)) };
            let payload = payload(result);
            let response: MachineInstanceCallResponse =
                plugin_api::decode_message(&payload).expect("decode command response");
            assert!(matches!(response.response, MachineResponse::Ack));
        }

        let save_req = plugin_api::encode_message(&MachineInstanceCallRequest {
            command: MachineCommand::SaveSnapshot {
                extension: "z80".to_string(),
            },
        })
        .expect("encode snapshot save request");
        // SAFETY: ABI function consumes request bytes only during call.
        let save_result =
            unsafe { plugin_instance_call_v1(create.instance, RcSlice::from_bytes(&save_req)) };
        let save_payload = payload(save_result);
        let save_response: MachineInstanceCallResponse =
            plugin_api::decode_message(&save_payload).expect("decode snapshot save response");
        let MachineResponse::SnapshotBlob(bytes) = save_response.response else {
            panic!("expected SnapshotBlob response");
        };
        assert!(!bytes.is_empty());

        // SAFETY: instance handle came from create response above.
        let status = unsafe { plugin_destroy_instance_v1(create.instance) };
        assert!(status.is_ok());
    }

    #[test]
    fn debug_chip_snapshots_expose_generic_overlay_chip_for_gui_panel() {
        let create_req = plugin_api::encode_message(&create_request("zx-spectrum-2a3"))
            .expect("encode create req");
        // SAFETY: ABI function consumes request bytes only during call.
        let create_result = unsafe { plugin_create_instance_v1(RcSlice::from_bytes(&create_req)) };
        let create_payload = payload(create_result);
        let create: MachineCreateResponse =
            plugin_api::decode_message(&create_payload).expect("decode create response");

        let call_req = MachineInstanceCallRequest {
            command: MachineCommand::Debug(DebugCommand::GetChipSnapshots),
        };
        let call_req_bytes = plugin_api::encode_message(&call_req).expect("encode call request");
        // SAFETY: ABI function consumes request bytes only during call.
        let call_result = unsafe {
            plugin_instance_call_v1(create.instance, RcSlice::from_bytes(&call_req_bytes))
        };
        let call_payload = payload(call_result);
        let call: MachineInstanceCallResponse =
            plugin_api::decode_message(&call_payload).expect("decode call response");

        let MachineResponse::Debug(DebugResponse::ChipSnapshots(snapshots)) = call.response else {
            panic!("expected debug chip snapshots response");
        };
        let overlay = snapshots
            .iter()
            .find(|chip| chip.chip_id == "overlay")
            .expect("generic overlay snapshot for the --show-overlay GUI panel");
        assert!(
            overlay.fields.iter().any(|f| f.key == "memory_summary"),
            "Spectrum2A3 has real paging -- overlay must report a memory_summary"
        );
        assert!(
            overlay.fields.iter().any(|f| f.key == "fdc_phase"),
            "Spectrum2A3 has a real FDC -- overlay must report fdc_phase"
        );
        assert!(
            overlay.fields.iter().any(|f| f.key == "disk_motor"),
            "Spectrum2A3 has a real disk motor line -- overlay must report disk_motor"
        );
        assert!(
            overlay.blobs.iter().any(|b| b.key == "tape_waveform"),
            "overlay blobs must include the (empty, no tape installed) tape_waveform key"
        );

        // SAFETY: instance handle came from create response above.
        let status = unsafe { plugin_destroy_instance_v1(create.instance) };
        assert!(status.is_ok());
    }

    #[test]
    fn debug_chip_snapshots_generic_overlay_omits_disk_fields_on_48k() {
        let create_req =
            plugin_api::encode_message(&create_request("zx-spectrum")).expect("encode create req");
        // SAFETY: ABI function consumes request bytes only during call.
        let create_result = unsafe { plugin_create_instance_v1(RcSlice::from_bytes(&create_req)) };
        let create_payload = payload(create_result);
        let create: MachineCreateResponse =
            plugin_api::decode_message(&create_payload).expect("decode create response");

        let call_req = MachineInstanceCallRequest {
            command: MachineCommand::Debug(DebugCommand::GetChipSnapshots),
        };
        let call_req_bytes = plugin_api::encode_message(&call_req).expect("encode call request");
        // SAFETY: ABI function consumes request bytes only during call.
        let call_result = unsafe {
            plugin_instance_call_v1(create.instance, RcSlice::from_bytes(&call_req_bytes))
        };
        let call_payload = payload(call_result);
        let call: MachineInstanceCallResponse =
            plugin_api::decode_message(&call_payload).expect("decode call response");

        let MachineResponse::Debug(DebugResponse::ChipSnapshots(snapshots)) = call.response else {
            panic!("expected debug chip snapshots response");
        };
        let overlay = snapshots
            .iter()
            .find(|chip| chip.chip_id == "overlay")
            .expect("generic overlay snapshot for the --show-overlay GUI panel");
        assert!(
            !overlay.fields.iter().any(|f| f.key == "memory_summary"),
            "48K has no paging concept -- overlay must NOT report memory_summary"
        );
        assert!(
            !overlay.fields.iter().any(|f| f.key == "fdc_phase"),
            "48K has no FDC -- overlay must NOT report fdc_phase"
        );
        assert!(
            !overlay.fields.iter().any(|f| f.key == "disk_motor"),
            "48K has no disk motor -- overlay must NOT report disk_motor"
        );

        // SAFETY: instance handle came from create response above.
        let status = unsafe { plugin_destroy_instance_v1(create.instance) };
        assert!(status.is_ok());
    }

    /// `firmware.toml` (next to the family's `Cargo.toml`) tells `--firmware auto` which file fills which machine
    /// option. An entry naming an option this plugin does not declare as a file option can never apply: this is
    /// exactly how the C64/C128 manifest once matched nothing (`rom_kernal` against the declared `kernal`).
    #[test]
    fn firmware_toml_names_only_file_options_the_plugin_declares() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../firmware.toml");
        let text = std::fs::read_to_string(path).expect("the family's firmware.toml");
        let manifest: toml::Value = text.parse().expect("firmware.toml is valid TOML");
        let profiles = machine_profiles();
        let mut checked = 0;
        for entry in manifest["rom"].as_array().expect("[[rom]] entries") {
            let option = entry["option"].as_str().expect("option = \"..\"");
            for machine in entry["machines"].as_array().expect("machines = [..]") {
                let id = machine.as_str().expect("a machine id");
                // Another plugin of the family serves the machines this one does not.
                let Some(profile) = profiles.iter().find(|p| p.machine_id == id) else {
                    continue;
                };
                let declared: Vec<&str> = profile
                    .options
                    .iter()
                    .filter(|o| o.kind == plugin_api::MachineOptionKind::File)
                    .map(|o| o.key.as_str())
                    .collect();
                assert!(
                    declared.contains(&option),
                    "firmware.toml: \"{}\" names option \"{option}\" for {id}, which declares the file options {declared:?}",
                    entry["label"].as_str().unwrap_or("?")
                );
                checked += 1;
            }
        }
        assert!(
            checked > 0,
            "no firmware.toml entry applies to this plugin's machines"
        );
    }
}
