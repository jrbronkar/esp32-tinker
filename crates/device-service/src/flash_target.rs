//! Writes are restricted to the inactive slot of the exact provisioned layout.
use embedded_storage::{ReadStorage, Storage};
use esp_bootloader_esp_idf::{
    ota::{Ota, OtaImageState},
    partitions::{
        read_partition_table, AppPartitionSubType as App, DataPartitionSubType as Data,
        PartitionType as Kind, PARTITION_TABLE_MAX_LEN,
    },
};
use esp_storage::FlashStorage;
use ota_core::{Error, Reader, Target, SLOT_SIZE};

pub struct FlashTarget {
    storage: FlashStorage<'static>,
    pub offset: u32,
    slot: App,
    pub available: bool,
}
impl FlashTarget {
    pub fn new(mut storage: FlashStorage<'static>) -> Self {
        let mut buffer = [0; PARTITION_TABLE_MAX_LEN];
        let mut available = false;
        let mut offset = 0;
        let mut slot = App::Ota0;
        if let Ok(table) = read_partition_table(&mut storage, &mut buffer) {
            let expected = [
                (Kind::Data(Data::Nvs), 0x9000, 0x6000),
                (Kind::Data(Data::Phy), 0xf000, 0x1000),
                (Kind::App(App::Ota0), 0x10000, SLOT_SIZE),
                (Kind::App(App::Ota1), 0x310000, SLOT_SIZE),
                (Kind::Data(Data::Ota), 0x610000, 0x2000),
            ];
            let layout = table.len() == expected.len()
                && storage.capacity() >= 0x612000
                && expected.iter().all(|(kind, start, size)| {
                    table
                        .iter()
                        .filter(|p| {
                            p.partition_type() == *kind
                                && p.offset() == *start
                                && p.len() == *size
                                && p.flags() == 0
                        })
                        .count()
                        == 1
                });
            if layout {
                if let Ok(Some(running)) = table.booted_partition() {
                    match running.partition_type() {
                        Kind::App(App::Ota0) => {
                            offset = 0x310000;
                            slot = App::Ota1;
                            available = true;
                        }
                        Kind::App(App::Ota1) => {
                            offset = 0x10000;
                            slot = App::Ota0;
                            available = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        Self {
            storage,
            offset,
            slot,
            available,
        }
    }
    /// Only confirm a bootloader trial when its selected slot is actually running.
    pub fn confirm_running(&mut self) -> Result<(), Error> {
        if !self.available {
            return Ok(());
        }
        let mut buffer = [0; PARTITION_TABLE_MAX_LEN];
        let table =
            read_partition_table(&mut self.storage, &mut buffer).map_err(|_| Error::Storage)?;
        let running = table
            .booted_partition()
            .map_err(|_| Error::Storage)?
            .ok_or(Error::State)?
            .partition_type();
        let part = table
            .find_partition(Kind::Data(Data::Ota))
            .map_err(|_| Error::Storage)?
            .ok_or(Error::State)?;
        let mut ota =
            Ota::new(part.as_embedded_storage(&mut self.storage), 2).map_err(|_| Error::Storage)?;
        if let Ok(selected) = ota.current_app_partition() {
            if Kind::App(selected) == running
                && ota.current_ota_state() == Ok(OtaImageState::PendingVerify)
            {
                ota.set_current_ota_state(OtaImageState::Valid)
                    .map_err(|_| Error::Storage)?;
            }
        }
        Ok(())
    }
}
impl Reader for FlashTarget {
    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Error> {
        if !self.available
            || offset
                .checked_add(bytes.len() as u32)
                .filter(|end| *end <= SLOT_SIZE)
                .is_none()
        {
            return Err(Error::Bounds);
        }
        self.storage
            .read(self.offset + offset, bytes)
            .map_err(|_| Error::Storage)
    }
}
impl Target for FlashTarget {
    fn capacity(&self) -> u32 {
        if self.available {
            SLOT_SIZE
        } else {
            0
        }
    }
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Error> {
        if !self.available
            || offset % 4096 != 0
            || bytes.len() > 4096
            || offset
                .checked_add(bytes.len() as u32)
                .filter(|end| *end <= SLOT_SIZE)
                .is_none()
        {
            return Err(Error::Bounds);
        }
        self.storage
            .write(self.offset + offset, bytes)
            .map_err(|_| Error::Storage)
    }
    fn commit(&mut self) -> Result<(), Error> {
        if !self.available {
            return Err(Error::State);
        }
        let mut buffer = [0; PARTITION_TABLE_MAX_LEN];
        let table =
            read_partition_table(&mut self.storage, &mut buffer).map_err(|_| Error::Storage)?;
        let running = table
            .booted_partition()
            .map_err(|_| Error::Storage)?
            .ok_or(Error::State)?;
        if running.offset() == self.offset {
            return Err(Error::State);
        }
        let part = table
            .find_partition(Kind::Data(Data::Ota))
            .map_err(|_| Error::Storage)?
            .ok_or(Error::State)?;
        let mut ota =
            Ota::new(part.as_embedded_storage(&mut self.storage), 2).map_err(|_| Error::Storage)?;
        ota.set_current_app_partition(self.slot)
            .map_err(|_| Error::Storage)?;
        ota.set_current_ota_state(OtaImageState::New)
            .map_err(|_| Error::Storage)
    }
}
