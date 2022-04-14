// jkcoxson

use std::sync::Arc;

use plist_plus::Plist;
use tokio::sync::Mutex;

use crate::central_data::CentralData;

pub async fn list_devices(data: Arc<Mutex<CentralData>>) -> Plist {
    let data = data.lock().await;
    let mut device_list = Plist::new_array();
    for i in &data.devices {
        let mut to_push = Plist::new_dict();
        to_push
            .dict_set_item("DeviceID", Plist::new_uint(i.1.device_id))
            .unwrap();
        to_push
            .dict_set_item("MessageType", "Attached".into())
            .unwrap();
        to_push
            .dict_set_item("Properties", i.1.try_into().unwrap())
            .unwrap();

        device_list.array_append_item(to_push).unwrap();
    }
    let mut upper = Plist::new_dict();
    upper.dict_set_item("DeviceList", device_list).unwrap();

    upper
}
