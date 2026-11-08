//! Captura en macOS con Core Audio.
//!
//! Las dos fuentes se leen igual: un IOProc del HAL sobre un dispositivo de
//! entrada, que entrega float32 a la frecuencia del dispositivo. Lo que cambia
//! es el dispositivo:
//!
//! - **Microfono**: el de entrada elegido (por su UID) o el predeterminado.
//! - **Audio del sistema**: macOS no tiene loopback como WASAPI. Desde macOS
//!   14.2 hay *process taps*: un `CATapDescription` global (todo lo que suena,
//!   sin excluir a nadie) se mete en un dispositivo agregado privado, y ese
//!   agregado se lee como si fuera un microfono. Es lo que pide permiso de
//!   "grabar el audio del sistema" (`NSAudioCaptureUsageDescription`).
//!
//! Las funciones del tap (`AudioHardwareCreateProcessTap`) y la clase
//! `CATapDescription` se buscan en tiempo de ejecucion: enlazarlas a pelo haria
//! que la app ni arrancase en un macOS anterior a 14.2, y asi solo falla la
//! captura del sistema, con un error que lo explica.
//!
//! El remuestreo a 16 kHz mono lo hace [`crate::resample::Resampler`] en el
//! hilo de captura; el IOProc solo hace la mezcla a mono y entrega el bloque,
//! para no trabajar de mas en el hilo de tiempo real.

#![allow(non_upper_case_globals, non_snake_case)]

use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{msg_send, sel};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUUID};

use crate::capture::{CaptureOpened, CaptureTarget, OnError, OnOpen};
use crate::device::{AudioDevice, DeviceKind};
use crate::resample::Resampler;
use crate::{AudioError, Result, TARGET_RATE};

/// 100 ms a 16 kHz, igual que la captura de Windows.
const SEND_SAMPLES: usize = TARGET_RATE as usize / 10;

/// Cuanto esperar audio antes de volver a mirar si hay que parar o si el
/// dispositivo sigue vivo.
const POLL: Duration = Duration::from_millis(200);

/// Id fijo de la unica "salida" que se ofrece: el tap es global y no depende
/// del dispositivo de salida que se elija.
pub const SYSTEM_TAP_ID: &str = "system-tap";

// ------------------------------------------------------------------- FFI

type OSStatus = i32;
type AudioObjectID = u32;
type AudioDeviceIOProcID = *mut c_void;
type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;

#[repr(C)]
struct AudioObjectPropertyAddress {
    mSelector: u32,
    mScope: u32,
    mElement: u32,
}

#[repr(C)]
struct AudioBuffer {
    mNumberChannels: u32,
    mDataByteSize: u32,
    mData: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    mNumberBuffers: u32,
    mBuffers: [AudioBuffer; 1],
}

/// Solo se recibe por puntero; su contenido no se usa.
#[repr(C)]
struct AudioTimeStamp {
    _private: [u8; 0],
}

type AudioDeviceIOProc = unsafe extern "C" fn(
    AudioObjectID,
    *const AudioTimeStamp,
    *const AudioBufferList,
    *const AudioTimeStamp,
    *mut AudioBufferList,
    *const AudioTimeStamp,
    *mut c_void,
) -> OSStatus;

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        out_size: *mut u32,
    ) -> OSStatus;
    fn AudioObjectGetPropertyData(
        object: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        io_size: *mut u32,
        out_data: *mut c_void,
    ) -> OSStatus;
    fn AudioDeviceCreateIOProcID(
        device: AudioObjectID,
        proc_: AudioDeviceIOProc,
        client_data: *mut c_void,
        out_id: *mut AudioDeviceIOProcID,
    ) -> OSStatus;
    fn AudioDeviceDestroyIOProcID(device: AudioObjectID, id: AudioDeviceIOProcID) -> OSStatus;
    fn AudioDeviceStart(device: AudioObjectID, id: AudioDeviceIOProcID) -> OSStatus;
    fn AudioDeviceStop(device: AudioObjectID, id: AudioDeviceIOProcID) -> OSStatus;
    fn AudioHardwareCreateAggregateDevice(
        description: CFDictionaryRef,
        out_device: *mut AudioObjectID,
    ) -> OSStatus;
    fn AudioHardwareDestroyAggregateDevice(device: AudioObjectID) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringGetLength(s: CFStringRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFRelease(cf: CFTypeRef);
}

extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

/// `RTLD_DEFAULT` en macOS: buscar en todas las imagenes cargadas.
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
const kCFStringEncodingUTF8: u32 = 0x0800_0100;

type CreateProcessTapFn = unsafe extern "C" fn(*mut AnyObject, *mut AudioObjectID) -> OSStatus;
type DestroyProcessTapFn = unsafe extern "C" fn(AudioObjectID) -> OSStatus;

const fn fourcc(code: &[u8; 4]) -> u32 {
    (code[0] as u32) << 24 | (code[1] as u32) << 16 | (code[2] as u32) << 8 | code[3] as u32
}

const kAudioObjectSystemObject: AudioObjectID = 1;
const kAudioObjectUnknown: AudioObjectID = 0;
const kAudioObjectPropertyScopeGlobal: u32 = fourcc(b"glob");
const kAudioObjectPropertyScopeInput: u32 = fourcc(b"inpt");
const kAudioObjectPropertyElementMain: u32 = 0;
const kAudioObjectPropertyName: u32 = fourcc(b"lnam");
const kAudioHardwarePropertyDevices: u32 = fourcc(b"dev#");
const kAudioHardwarePropertyDefaultInputDevice: u32 = fourcc(b"dIn ");
const kAudioHardwarePropertyDefaultOutputDevice: u32 = fourcc(b"dOut");
const kAudioHardwarePropertyTranslatePIDToProcessObject: u32 = fourcc(b"id2p");
const kAudioDevicePropertyDeviceUID: u32 = fourcc(b"uid ");
const kAudioDevicePropertyDeviceIsAlive: u32 = fourcc(b"livn");
const kAudioDevicePropertyNominalSampleRate: u32 = fourcc(b"nsrt");
const kAudioDevicePropertyStreamConfiguration: u32 = fourcc(b"slay");

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Un OSStatus suele ser un four-char code; asi se lee en el log.
fn status_text(status: OSStatus) -> String {
    let bytes = (status as u32).to_be_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        format!("'{}' ({status})", String::from_utf8_lossy(&bytes))
    } else {
        status.to_string()
    }
}

fn check(status: OSStatus, what: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(AudioError::CoreAudio(format!(
            "{what} failed: {}",
            status_text(status)
        )))
    }
}

// --------------------------------------------------------- propiedades

fn get_u32(object: AudioObjectID, selector: u32, scope: u32) -> Result<u32> {
    let addr = address(selector, scope);
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    check(
        unsafe {
            AudioObjectGetPropertyData(
                object,
                &addr,
                0,
                ptr::null(),
                &mut size,
                &mut value as *mut u32 as *mut c_void,
            )
        },
        "AudioObjectGetPropertyData",
    )?;
    Ok(value)
}

fn get_f64(object: AudioObjectID, selector: u32, scope: u32) -> Result<f64> {
    let addr = address(selector, scope);
    let mut value: f64 = 0.0;
    let mut size = std::mem::size_of::<f64>() as u32;
    check(
        unsafe {
            AudioObjectGetPropertyData(
                object,
                &addr,
                0,
                ptr::null(),
                &mut size,
                &mut value as *mut f64 as *mut c_void,
            )
        },
        "AudioObjectGetPropertyData",
    )?;
    Ok(value)
}

/// Lee una propiedad CFString (nombre, UID) y la libera.
fn get_string(object: AudioObjectID, selector: u32) -> Result<String> {
    let addr = address(selector, kAudioObjectPropertyScopeGlobal);
    let mut cf: CFStringRef = ptr::null();
    let mut size = std::mem::size_of::<CFStringRef>() as u32;
    check(
        unsafe {
            AudioObjectGetPropertyData(
                object,
                &addr,
                0,
                ptr::null(),
                &mut size,
                &mut cf as *mut CFStringRef as *mut c_void,
            )
        },
        "AudioObjectGetPropertyData",
    )?;
    if cf.is_null() {
        return Ok(String::new());
    }
    let text = unsafe {
        let len = CFStringGetLength(cf);
        let cap = CFStringGetMaximumSizeForEncoding(len, kCFStringEncodingUTF8) + 1;
        let mut buf = vec![0 as c_char; cap.max(1) as usize];
        let ok = CFStringGetCString(cf, buf.as_mut_ptr(), cap, kCFStringEncodingUTF8);
        CFRelease(cf);
        if ok == 0 {
            String::new()
        } else {
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        }
    };
    Ok(text)
}

fn all_devices() -> Result<Vec<AudioObjectID>> {
    let addr = address(kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal);
    let mut size: u32 = 0;
    check(
        unsafe {
            AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &addr, 0, ptr::null(), &mut size)
        },
        "listing audio devices",
    )?;
    let mut ids = vec![0 as AudioObjectID; size as usize / std::mem::size_of::<AudioObjectID>()];
    check(
        unsafe {
            AudioObjectGetPropertyData(
                kAudioObjectSystemObject,
                &addr,
                0,
                ptr::null(),
                &mut size,
                ids.as_mut_ptr() as *mut c_void,
            )
        },
        "listing audio devices",
    )?;
    ids.truncate(size as usize / std::mem::size_of::<AudioObjectID>());
    Ok(ids)
}

/// Canales de entrada de un dispositivo (0 si solo es de salida).
fn input_channels(device: AudioObjectID) -> u32 {
    let addr = address(kAudioDevicePropertyStreamConfiguration, kAudioObjectPropertyScopeInput);
    let mut size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(device, &addr, 0, ptr::null(), &mut size)
    };
    if status != 0 || size == 0 {
        return 0;
    }
    // u64 para que el AudioBufferList quede alineado como pide su puntero.
    let mut storage = vec![0u64; (size as usize).div_ceil(8)];
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            &addr,
            0,
            ptr::null(),
            &mut size,
            storage.as_mut_ptr() as *mut c_void,
        )
    };
    if status != 0 {
        return 0;
    }
    unsafe { buffers(storage.as_ptr() as *const AudioBufferList) }
        .iter()
        .map(|b| b.mNumberChannels)
        .sum()
}

fn is_alive(device: AudioObjectID) -> bool {
    get_u32(device, kAudioDevicePropertyDeviceIsAlive, kAudioObjectPropertyScopeGlobal)
        .map(|alive| alive != 0)
        .unwrap_or(false)
}

unsafe fn buffers<'a>(list: *const AudioBufferList) -> &'a [AudioBuffer] {
    std::slice::from_raw_parts((*list).mBuffers.as_ptr(), (*list).mNumberBuffers as usize)
}

// ------------------------------------------------------------ listado

pub fn list_devices(kind: DeviceKind) -> Result<Vec<AudioDevice>> {
    match kind {
        // El tap es global: se ofrece una sola "salida" que lo representa.
        DeviceKind::Output => Ok(vec![AudioDevice {
            id: SYSTEM_TAP_ID.to_string(),
            name: "System audio (all apps)".to_string(),
            kind,
            is_default: true,
        }]),
        DeviceKind::Input => {
            let default = get_u32(
                kAudioObjectSystemObject,
                kAudioHardwarePropertyDefaultInputDevice,
                kAudioObjectPropertyScopeGlobal,
            )
            .unwrap_or(kAudioObjectUnknown);
            let mut list = Vec::new();
            for device in all_devices()? {
                if input_channels(device) == 0 {
                    continue;
                }
                let Ok(id) = get_string(device, kAudioDevicePropertyDeviceUID) else {
                    continue;
                };
                // Los agregados privados (los de los taps) no tienen nombre
                // util y no son microfonos.
                let name = get_string(device, kAudioObjectPropertyName).unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                list.push(AudioDevice {
                    id,
                    name,
                    kind,
                    is_default: device == default,
                });
            }
            Ok(list)
        }
    }
}

// ------------------------------------------------------------- IOProc

/// Mezcla a mono lo que llega y lo manda al hilo de captura.
unsafe extern "C" fn io_proc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    _input_time: *const AudioTimeStamp,
    _output: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> OSStatus {
    if input.is_null() || client.is_null() {
        return 0;
    }
    let tx = &*(client as *const Sender<Vec<f32>>);
    let mut mono: Vec<f32> = Vec::new();
    let mut channels = 0usize;
    for buffer in buffers(input) {
        let ch = buffer.mNumberChannels as usize;
        if buffer.mData.is_null() || ch == 0 {
            continue;
        }
        let frames = buffer.mDataByteSize as usize / std::mem::size_of::<f32>() / ch;
        let data = std::slice::from_raw_parts(buffer.mData as *const f32, frames * ch);
        if mono.is_empty() {
            mono = vec![0.0; frames];
        }
        for (i, out) in mono.iter_mut().enumerate().take(frames) {
            *out += data[i * ch..(i + 1) * ch].iter().sum::<f32>();
        }
        channels += ch;
    }
    if channels == 0 || mono.is_empty() {
        return 0;
    }
    let scale = 1.0 / channels as f32;
    for s in &mut mono {
        *s *= scale;
    }
    let _ = tx.send(mono);
    0
}

/// Un IOProc arrancado sobre un dispositivo. Al soltarlo se para y se libera.
struct IoReader {
    device: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    client: *mut Sender<Vec<f32>>,
}

impl IoReader {
    fn start(device: AudioObjectID) -> Result<(Self, Receiver<Vec<f32>>)> {
        let (tx, rx) = mpsc::channel();
        let client = Box::into_raw(Box::new(tx));
        let mut proc_id: AudioDeviceIOProcID = ptr::null_mut();
        let status = unsafe {
            AudioDeviceCreateIOProcID(device, io_proc, client as *mut c_void, &mut proc_id)
        };
        if let Err(e) = check(status, "AudioDeviceCreateIOProcID") {
            drop(unsafe { Box::from_raw(client) });
            return Err(e);
        }
        let reader = IoReader {
            device,
            proc_id,
            client,
        };
        // Si no arranca, el Drop deshace lo creado.
        check(unsafe { AudioDeviceStart(device, proc_id) }, "AudioDeviceStart")?;
        Ok((reader, rx))
    }
}

impl Drop for IoReader {
    fn drop(&mut self) {
        unsafe {
            // Al volver AudioDeviceStop el IOProc ya no se llama: liberar el
            // canal despues es seguro.
            AudioDeviceStop(self.device, self.proc_id);
            AudioDeviceDestroyIOProcID(self.device, self.proc_id);
            drop(Box::from_raw(self.client));
        }
    }
}

// -------------------------------------------------------- process tap

/// Tap + dispositivo agregado que lo expone. Al soltarlo se destruyen.
struct ProcessTap {
    tap: AudioObjectID,
    aggregate: AudioObjectID,
    destroy_tap: DestroyProcessTapFn,
}

impl Drop for ProcessTap {
    fn drop(&mut self) {
        unsafe {
            AudioHardwareDestroyAggregateDevice(self.aggregate);
            (self.destroy_tap)(self.tap);
        }
    }
}

fn unsupported_tap() -> AudioError {
    AudioError::CoreAudio(
        "capturing system audio needs macOS 14.2 or later (Core Audio process taps)".to_string(),
    )
}

fn symbol(name: &CStr) -> *mut c_void {
    unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) }
}

fn responds(object: &AnyObject, selector: objc2::runtime::Sel) -> bool {
    unsafe { msg_send![object, respondsToSelector: selector] }
}

/// Crea un tap y lo envuelve en un agregado privado que se puede leer.
/// `processes`: `None` = todo el sistema; si no, solo esos objetos de proceso.
fn create_tap(processes: Option<&[AudioObjectID]>) -> Result<ProcessTap> {
    let create = symbol(c"AudioHardwareCreateProcessTap");
    let destroy = symbol(c"AudioHardwareDestroyProcessTap");
    let class = AnyClass::get(c"CATapDescription");
    let (Some(class), false, false) = (class, create.is_null(), destroy.is_null()) else {
        return Err(unsupported_tap());
    };
    let create: CreateProcessTapFn = unsafe { std::mem::transmute(create) };
    let destroy_tap: DestroyProcessTapFn = unsafe { std::mem::transmute(destroy) };

    let description: Retained<AnyObject> = unsafe {
        let allocated: Allocated<AnyObject> = msg_send![class, alloc];
        let initialized: Option<Retained<AnyObject>> = match processes {
            None => {
                let none: Retained<NSArray<NSNumber>> = NSArray::new();
                msg_send![allocated, initStereoGlobalTapButExcludeProcesses: &*none]
            }
            Some(ids) => {
                let numbers: Vec<Retained<NSNumber>> =
                    ids.iter().map(|id| NSNumber::new_u32(*id)).collect();
                let list = NSArray::from_retained_slice(&numbers);
                msg_send![allocated, initStereoMixdownOfProcesses: &*list]
            }
        };
        initialized.ok_or_else(|| AudioError::CoreAudio("CATapDescription init failed".into()))?
    };

    // Un UUID propio, para poder citar el tap desde el agregado.
    let uuid = NSUUID::new();
    if responds(&description, sel!(setUUID:)) {
        let _: () = unsafe { msg_send![&*description, setUUID: &*uuid] };
    }
    // Privado: no aparece como dispositivo para otras apps.
    // La propiedad es `privateTap` (getter `isPrivate`); se tantean los dos
    // nombres de setter que se han visto en las cabeceras.
    if responds(&description, sel!(setPrivateTap:)) {
        let _: () = unsafe { msg_send![&*description, setPrivateTap: true] };
    } else if responds(&description, sel!(setPrivate:)) {
        let _: () = unsafe { msg_send![&*description, setPrivate: true] };
    }
    let tap_uid: Retained<NSString> = if responds(&description, sel!(UUID)) {
        let current: Retained<NSUUID> = unsafe { msg_send![&*description, UUID] };
        current.UUIDString()
    } else {
        uuid.UUIDString()
    };

    let mut tap: AudioObjectID = kAudioObjectUnknown;
    check(
        unsafe { create(Retained::as_ptr(&description) as *mut AnyObject, &mut tap) },
        "AudioHardwareCreateProcessTap",
    )?;

    match create_aggregate(&tap_uid) {
        Ok(aggregate) => Ok(ProcessTap {
            tap,
            aggregate,
            destroy_tap,
        }),
        Err(e) => {
            unsafe { destroy_tap(tap) };
            Err(e)
        }
    }
}

fn create_aggregate(tap_uid: &NSString) -> Result<AudioObjectID> {
    let key = |k: &str| NSString::from_str(k);
    let yes = NSNumber::new_bool(true);
    let no = NSNumber::new_bool(false);

    // Constantes de AudioHardware.h: kAudioSubTapUIDKey = "uid",
    // kAudioSubTapDriftCompensationKey = "drift".
    let tap_keys = [key("uid"), key("drift")];
    let tap_values: [&AnyObject; 2] = [tap_uid, &yes];
    let tap_entry: Retained<NSDictionary<NSString, AnyObject>> =
        NSDictionary::from_slices(&[&*tap_keys[0], &*tap_keys[1]], &tap_values);
    let taps = NSArray::from_retained_slice(&[tap_entry]);

    let name = NSString::from_str("LiveTranscriberLite system audio");
    let uid = NSUUID::new().UUIDString();

    // kAudioAggregateDevice{Name,UID,IsPrivate,IsStacked,TapAutoStart,TapList}Key
    let mut keys = vec![
        key("name"),
        key("uid"),
        key("private"),
        key("stacked"),
        key("tapautostart"),
        key("taps"),
    ];
    let mut values: Vec<&AnyObject> = vec![&name, &uid, &yes, &no, &yes, &taps];

    // La salida predeterminada como dispositivo principal, para que el
    // agregado tenga reloj. Sin ella el tap funciona igual en la mayoria de
    // casos, asi que si falla se sigue sin ella.
    let output_uid = get_u32(
        kAudioObjectSystemObject,
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
    )
    .and_then(|device| get_string(device, kAudioDevicePropertyDeviceUID))
    .ok()
    .filter(|uid| !uid.is_empty())
    .map(|uid| NSString::from_str(&uid));
    let sub_key = key("uid");
    let sub_entry;
    let subdevices;
    if let Some(output_uid) = &output_uid {
        let sub_values: [&AnyObject; 1] = [output_uid];
        sub_entry = NSDictionary::<NSString, AnyObject>::from_slices(&[&*sub_key], &sub_values);
        subdevices = NSArray::from_retained_slice(&[sub_entry]);
        // kAudioAggregateDeviceMainSubDeviceKey = "master",
        // kAudioAggregateDeviceSubDeviceListKey = "subdevices".
        keys.push(key("master"));
        values.push(output_uid);
        keys.push(key("subdevices"));
        values.push(&subdevices);
    }

    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let description: Retained<NSDictionary<NSString, AnyObject>> =
        NSDictionary::from_slices(&key_refs, &values);

    let mut aggregate: AudioObjectID = kAudioObjectUnknown;
    check(
        unsafe {
            AudioHardwareCreateAggregateDevice(
                Retained::as_ptr(&description) as CFDictionaryRef,
                &mut aggregate,
            )
        },
        "AudioHardwareCreateAggregateDevice",
    )?;
    Ok(aggregate)
}

fn process_object(pid: u32) -> Result<AudioObjectID> {
    let addr = address(
        kAudioHardwarePropertyTranslatePIDToProcessObject,
        kAudioObjectPropertyScopeGlobal,
    );
    let pid = pid as i32;
    let mut object: AudioObjectID = kAudioObjectUnknown;
    let mut size = std::mem::size_of::<AudioObjectID>() as u32;
    check(
        unsafe {
            AudioObjectGetPropertyData(
                kAudioObjectSystemObject,
                &addr,
                std::mem::size_of::<i32>() as u32,
                &pid as *const i32 as *const c_void,
                &mut size,
                &mut object as *mut AudioObjectID as *mut c_void,
            )
        },
        "translating the pid to a Core Audio process",
    )?;
    if object == kAudioObjectUnknown {
        return Err(AudioError::CoreAudio(format!(
            "process {pid} is not playing audio through Core Audio"
        )));
    }
    Ok(object)
}

// ------------------------------------------------------------- captura

/// Lo que hay que mantener vivo mientras se lee: el tap (si lo hay) y el
/// dispositivo del que leer.
struct Opened {
    info: CaptureOpened,
    device: AudioObjectID,
    rate: f64,
    _tap: Option<ProcessTap>,
}

fn find_input(uid: &str) -> Option<AudioObjectID> {
    all_devices().ok()?.into_iter().find(|&device| {
        input_channels(device) > 0
            && get_string(device, kAudioDevicePropertyDeviceUID).is_ok_and(|found| found == uid)
    })
}

fn open(target: &CaptureTarget) -> Result<Opened> {
    match target {
        CaptureTarget::Microphone { device_id } => {
            let wanted = device_id.as_deref().filter(|id| !id.is_empty());
            let found = wanted.and_then(find_input);
            let fallback_from = match (wanted, found) {
                (Some(id), None) => Some(id.to_string()),
                _ => None,
            };
            let device = match found {
                Some(device) => device,
                None => {
                    let default = get_u32(
                        kAudioObjectSystemObject,
                        kAudioHardwarePropertyDefaultInputDevice,
                        kAudioObjectPropertyScopeGlobal,
                    )?;
                    if default == kAudioObjectUnknown {
                        return Err(AudioError::DeviceNotFound("default input".to_string()));
                    }
                    default
                }
            };
            let rate = get_f64(
                device,
                kAudioDevicePropertyNominalSampleRate,
                kAudioObjectPropertyScopeGlobal,
            )?;
            Ok(Opened {
                info: CaptureOpened {
                    device_name: get_string(device, kAudioObjectPropertyName).unwrap_or_default(),
                    device_id: get_string(device, kAudioDevicePropertyDeviceUID)
                        .unwrap_or_default(),
                    fallback_from,
                },
                device,
                rate,
                _tap: None,
            })
        }
        CaptureTarget::Loopback { device_id } => {
            if let Some(id) = device_id.as_deref().filter(|id| !id.is_empty() && *id != SYSTEM_TAP_ID) {
                tracing::info!("macOS captura todo el audio del sistema; se ignora la salida {id}");
            }
            let tap = create_tap(None)?;
            tap_opened(tap, "System audio (all apps)")
        }
        CaptureTarget::Process {
            pid,
            include_children,
        } => {
            if *include_children {
                tracing::info!("macOS: se captura solo el proceso {pid}, sin sus hijos");
            }
            let object = process_object(*pid)?;
            let tap = create_tap(Some(&[object]))?;
            tap_opened(tap, &format!("Process {pid}"))
        }
    }
}

fn tap_opened(tap: ProcessTap, name: &str) -> Result<Opened> {
    let rate = get_f64(
        tap.aggregate,
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal,
    )?;
    Ok(Opened {
        info: CaptureOpened {
            device_name: name.to_string(),
            device_id: SYSTEM_TAP_ID.to_string(),
            fallback_from: None,
        },
        device: tap.aggregate,
        rate,
        _tap: Some(tap),
    })
}

/// Misma promesa que la version de Windows: un hilo que entrega bloques de
/// 100 ms a 16 kHz mono hasta que `running` pasa a false o el receptor se va.
pub fn spawn_capture(
    target: CaptureTarget,
    running: Arc<AtomicBool>,
    tx: SyncSender<Vec<f32>>,
    on_open: Option<OnOpen>,
    on_error: Option<OnError>,
) -> Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("asr-capture".into())
        .spawn(move || {
            if let Err(e) = capture_loop(&target, &running, &tx, on_open) {
                tracing::error!("captura detenida: {e}");
                if let Some(on_error) = on_error {
                    on_error(e.to_string());
                }
            }
            running.store(false, Ordering::Relaxed);
            tracing::info!("hilo de captura terminado");
        })
        .map_err(|e| AudioError::Thread(e.to_string()))
}

fn capture_loop(
    target: &CaptureTarget,
    running: &AtomicBool,
    tx: &SyncSender<Vec<f32>>,
    on_open: Option<OnOpen>,
) -> Result<()> {
    let opened = open(target)?;
    if opened.rate.is_nan() || opened.rate <= 0.0 {
        return Err(AudioError::CoreAudio(format!(
            "the device reports a sample rate of {}",
            opened.rate
        )));
    }
    // El lector se suelta antes que el tap: el orden de declaracion lo asegura.
    let (reader, rx) = IoReader::start(opened.device)?;
    tracing::info!(
        "captura iniciada: {} [{}] ({target:?}, {} Hz)",
        opened.info.device_name,
        opened.info.device_id,
        opened.rate
    );
    if let Some(on_open) = on_open {
        on_open(opened.info.clone());
    }

    let mut resampler = Resampler::new(opened.rate, TARGET_RATE as f64);
    let mut pending: Vec<f32> = Vec::with_capacity(SEND_SAMPLES * 2);
    while running.load(Ordering::Relaxed) {
        match rx.recv_timeout(POLL) {
            Ok(block) => {
                resampler.process(&block, &mut pending);
                while pending.len() >= SEND_SAMPLES {
                    let chunk: Vec<f32> = pending.drain(..SEND_SAMPLES).collect();
                    if tx.send(chunk).is_err() {
                        return Ok(());
                    }
                }
            }
            // Un tap en silencio sigue entregando ceros; un microfono
            // desenchufado deja de llamar al IOProc. Lo segundo hay que decirlo.
            Err(RecvTimeoutError::Timeout) => {
                if !is_alive(opened.device) {
                    return Err(AudioError::CoreAudio(
                        "the audio device was disconnected".to_string(),
                    ));
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(reader);
    Ok(())
}
