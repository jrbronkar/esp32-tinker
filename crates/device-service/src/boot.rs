//! Startup-only flash reads, before starting the radio. No erase/write calls.
use embedded_storage::ReadStorage;
use esp_bootloader_esp_idf::partitions::{
    read_partition_table, AppPartitionSubType, DataPartitionSubType, PartitionEntry, PartitionType,
    PARTITION_TABLE_MAX_LEN,
};
use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;
use ota_protocol::{BootInfo, PartitionInfo};

pub fn inspect(flash: FLASH<'static>) -> (BootInfo, FlashStorage<'static>) {
    let mut storage = FlashStorage::new(flash);
    let report = inspect_storage(&mut storage);
    (report, storage)
}

fn inspect_storage(storage: &mut FlashStorage<'_>) -> BootInfo {
    let mut report = BootInfo {
        configured_flash_size_bytes: storage.capacity() as u32,
        ..Default::default()
    };
    let mut buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = match read_partition_table(storage, &mut buffer) {
        Ok(table) => table,
        Err(_) => {
            report.error = "unable to read/validate partition table"
                .try_into()
                .unwrap();
            return report;
        }
    };
    report.partition_count = table.len() as u32;
    for entry in table.iter() {
        match entry.partition_type() {
            PartitionType::App(AppPartitionSubType::Ota0) => {
                report.ota_app_partition_count += 1;
                report.set_ota_0(partition(entry));
            }
            PartitionType::App(AppPartitionSubType::Ota1) => {
                report.ota_app_partition_count += 1;
                report.set_ota_1(partition(entry));
            }
            PartitionType::App(AppPartitionSubType::Factory | AppPartitionSubType::Test) => {}
            PartitionType::App(_) => report.ota_app_partition_count += 1,
            PartitionType::Data(DataPartitionSubType::Ota) => {
                report.has_ota_data = entry.len() == 0x2000;
            }
            _ => {}
        }
    }
    match table.booted_partition() {
        Ok(Some(entry)) => {
            report.set_running(partition(entry));
        }
        _ => {
            report.error = "running partition could not be identified"
                .try_into()
                .unwrap();
            return report;
        }
    }
    report.layout_valid = true;
    report
}

fn partition(entry: PartitionEntry<'_>) -> PartitionInfo {
    // ESP partition labels occupy at most 16 bytes. Avoid label_as_str's panic
    // if a corrupt or non-UTF8 label made it through the table reader.
    let raw_label = entry.label();
    let end = raw_label
        .iter()
        .position(|b| *b == 0 || *b == 0xff)
        .unwrap_or(raw_label.len());
    let label = core::str::from_utf8(&raw_label[..end]).unwrap_or("<invalid>");
    PartitionInfo {
        label: label.try_into().unwrap_or_default(),
        offset: entry.offset(),
        size: entry.len(),
    }
}
