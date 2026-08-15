use nexo_core::{FileChunk, FileTransferOffer};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const FILE_TRANSFER_PROTOCOL: &str = "/nexo/file-transfer/0.1.0";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FileTransferRequest {
    Offer(FileTransferOffer),
    GetChunk { transfer_id: Uuid, chunk_index: u32 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FileTransferResponse {
    OfferAccepted { transfer_id: Uuid },
    OfferRejected { reason: String },
    Chunk(FileChunk),
    ChunkNotFound,
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexo_core::DeviceIdentity;

    #[test]
    fn file_transfer_request_response_roundtrip() {
        let identity = DeviceIdentity::generate();
        let offer = FileTransferOffer::create(
            &identity,
            Uuid::new_v4(),
            Uuid::new_v4(),
            "test.bin".into(),
            1024,
            "application/octet-stream".into(),
            [0u8; 32],
            1_700_000_000,
        )
        .expect("offer created");

        let req = FileTransferRequest::Offer(offer.clone());
        let res = FileTransferResponse::OfferAccepted {
            transfer_id: offer.id,
        };

        assert_eq!(
            res,
            FileTransferResponse::OfferAccepted {
                transfer_id: offer.id
            }
        );
        assert!(matches!(req, FileTransferRequest::Offer(_)));
    }
}
