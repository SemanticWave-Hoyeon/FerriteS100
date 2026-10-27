use super::*;

use crate::reader::tests::{KNOWN, S124, S129};

#[test]
fn file_uris() {
    assert_eq!(
        resolve("file:/S-124/DATASET_FILES/124GB00X.GML").as_deref(),
        Some("S-124/DATASET_FILES/124GB00X.GML")
    );
    assert_eq!(
        resolve("file::124GB00X.TXT").as_deref(),
        Some("124GB00X.TXT")
    );
    assert_eq!(
        resolve("file:///S-129\\A.gml").as_deref(),
        Some("S-129/A.gml")
    );
    assert_eq!(resolve("file:/S-124/../../etc/passwd"), None);
    assert_eq!(resolve("file:/C:/x.gml"), None);
    assert_eq!(resolve(""), None);
    assert_eq!(
        product_of(None, Some("124GB00X.GML"), &KNOWN),
        Some(S124.id)
    );
    assert_eq!(
        product_of(None, Some("S-129/DATASET_FILES/x.gml"), &KNOWN),
        Some(S129.id)
    );
    assert_eq!(product_of(Some("S-101"), Some("S-124/x"), &KNOWN), None);
}
