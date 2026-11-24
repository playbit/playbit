//! One capability namespace per guest thread, shared by every execution adapter.
//! Only operation payloads differ between adapters; allocation, kinds, rights,
//! File ownership, volume authorization and channel identity share this owner.
use super::{
    file_handles::Entry,
    host::{AvailableVolume, HostResult},
};
use crate::capability::{Budget, Handle, Rights, Table};
use playbit_api::abi::syscall::Error;
use std::rc::Rc;

pub const VOLUME: u16 = 1;
pub const FILE: u16 = 2;
pub const LIST: u16 = 3;
pub const OPERATION: u16 = 4;
pub const CHANNEL: u16 = 5;
#[cfg(all(feature = "native-audio", target_vendor = "apple"))]
pub const AUDIO: u16 = 6;
#[cfg(all(feature = "native-audio", target_vendor = "apple"))]
pub const SOUND: u16 = 7;
pub enum Object<P> {
    Volume(Rc<AvailableVolume>),
    File(Rc<Entry>),
    Operation(Rc<P>),
    Channel(Rc<String>),
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    Audio(Rc<super::audio_handles::Device>),
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    Sound(Rc<super::audio_handles::Sound>),
}
pub struct Capabilities<P> {
    pub table: Table<Object<P>>,
}
impl<P> Default for Capabilities<P> {
    fn default() -> Self {
        Self::new()
    }
}
impl<P> Capabilities<P> {
    pub fn new() -> Self {
        Self { table: Table::new(Budget::guest()) }
    }
    pub fn count(&self, kind: u16) -> usize {
        self.table.count(kind)
    }
    pub fn reserve(&mut self, kind: u16, rights: Rights) -> HostResult<Handle> {
        self.table.reserve(kind, rights)
    }
    pub fn cancel(&mut self, handle: Handle) -> HostResult<()> {
        self.table.cancel(handle)
    }
    pub fn volume(&self, handle: Handle, rights: Rights) -> HostResult<&AvailableVolume> {
        match self.table.get(handle, VOLUME, rights)? {
            Object::Volume(volume) => Ok(volume),
            _ => unreachable!("volume kind owns volume object"),
        }
    }
    pub fn add_volume(&mut self, volume: AvailableVolume) -> HostResult<Handle> {
        self.table.allocate(VOLUME, volume.data_rights(), Object::Volume(Rc::new(volume)))
    }
    pub fn replace_volume(&mut self, handle: Handle, volume: AvailableVolume) -> HostResult<()> {
        self.table.update_rights(handle, VOLUME, volume.data_rights())?;
        *self.table.get_mut(handle, VOLUME, Rights::NONE)? = Object::Volume(Rc::new(volume));
        Ok(())
    }
    pub fn volumes(&self) -> impl Iterator<Item = (Handle, &AvailableVolume)> {
        self.table.entries().filter_map(|(id, _, _, object)| match object {
            Object::Volume(v) => Some((id, v.as_ref())),
            _ => None,
        })
    }
    pub fn file(&self, handle: Handle, list: bool) -> HostResult<Rc<Entry>> {
        match self.table.get(handle, if list { LIST } else { FILE }, Rights::NONE)? {
            Object::File(file) => Ok(file.clone()),
            _ => unreachable!("file kind owns File object"),
        }
    }
    pub fn files(&self) -> impl Iterator<Item = &Entry> {
        self.table.values().filter_map(|object| match object {
            Object::File(file) => Some(file.as_ref()),
            _ => None,
        })
    }
    pub fn insert_file(&mut self, handle: Handle, entry: Entry) -> HostResult<()> {
        let kind = if entry.resource.kind == super::file_handles::Kind::List { LIST } else { FILE };
        let rights = match entry.resource.kind {
            super::file_handles::Kind::Directory => entry.resource.rights,
            super::file_handles::Kind::Writer => Rights::WRITE,
            _ => Rights::READ,
        };
        self.table.insert(handle, Object::File(Rc::new(entry)))?;
        self.table.update_rights(handle, kind, rights)
    }
    pub fn remove_file(&mut self, handle: Handle, list: bool) -> HostResult<Rc<Entry>> {
        match self.table.remove(handle, if list { LIST } else { FILE })? {
            Object::File(file) => Ok(file),
            _ => unreachable!("file kind owns File object"),
        }
    }
    pub fn operation(&self, handle: Handle) -> HostResult<&P> {
        match self.table.get(handle, OPERATION, Rights::NONE)? {
            Object::Operation(operation) => Ok(operation),
            _ => unreachable!("operation kind owns operation object"),
        }
    }
    pub fn insert_operation(&mut self, handle: Handle, operation: P) -> HostResult<()> {
        self.table.insert(handle, Object::Operation(Rc::new(operation)))
    }
    pub fn remove_operation(&mut self, handle: Handle) -> HostResult<Rc<P>> {
        match self.table.remove(handle, OPERATION)? {
            Object::Operation(operation) => Ok(operation),
            _ => unreachable!("operation kind owns operation object"),
        }
    }
    pub fn operations(&self) -> impl Iterator<Item = (Handle, &P)> {
        self.table.entries().filter_map(|(id, _, _, object)| match object {
            Object::Operation(p) => Some((id, p.as_ref())),
            _ => None,
        })
    }
    pub fn channel(&self, handle: Handle) -> HostResult<&str> {
        match self.table.get(handle, CHANNEL, Rights::NONE)? {
            Object::Channel(name) => Ok(name),
            _ => unreachable!("channel kind owns channel name"),
        }
    }
    pub fn channel_for_action(&self, handle: Handle, action: u32) -> HostResult<&str> {
        let rights = crate::session_channel::command_rights(action)?;
        match self.table.get(handle, CHANNEL, rights)? {
            Object::Channel(name) => Ok(name),
            _ => unreachable!("channel kind owns channel name"),
        }
    }
    pub fn find_channel(&self, name: &str) -> Option<Handle> {
        self.table.entries().find_map(|(id, _, _, object)| match object {
            Object::Channel(n) if n.as_str() == name => Some(id),
            _ => None,
        })
    }
    pub fn open_channel(&mut self, name: &str) -> HostResult<Handle> {
        crate::session_channel::validate_name(name).map_err(|_| Error::InvalidArgument)?;
        if let Some(handle) = self.find_channel(name) {
            return Ok(handle);
        }
        if self.count(CHANNEL) == crate::session_channel::MAX_CHANNELS {
            return Err(Error::LimitExceeded);
        }
        self.table.allocate(
            CHANNEL,
            Rights(Rights::READ.0 | Rights::WRITE.0),
            Object::Channel(Rc::new(name.into())),
        )
    }
    pub fn close_channel(&mut self, handle: Handle) -> HostResult<()> {
        self.table.remove(handle, CHANNEL).map(|_| ())
    }
}

#[cfg(all(feature = "native-audio", target_vendor = "apple"))]
impl<P> Capabilities<P> {
    pub(in crate::runtime) fn audio(
        &self,
        handle: Handle,
        rights: Rights,
    ) -> HostResult<&Rc<super::audio_handles::Device>> {
        match self.table.get(handle, AUDIO, rights)? {
            Object::Audio(device) => {
                device.require()?;
                Ok(device)
            }
            _ => unreachable!("audio kind owns audio endpoint"),
        }
    }
    pub(in crate::runtime) fn sound(
        &self,
        handle: Handle,
        rights: Rights,
    ) -> HostResult<&Rc<super::audio_handles::Sound>> {
        match self.table.get(handle, SOUND, rights)? {
            Object::Sound(sound) => {
                sound.device.require()?;
                Ok(sound)
            }
            _ => unreachable!("sound kind owns sound resource"),
        }
    }
}
