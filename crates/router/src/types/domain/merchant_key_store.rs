use common_utils::{
    crypto::{Encryptable, GcmAes256},
    custom_serde, date_time,
};
use error_stack::ResultExt;
use masking::Secret;
use time::PrimitiveDateTime;

use crate::{
    db::StorageInterface,
    errors::{CustomResult, ValidationError},
    types::domain::types::TypeEncryption,
};

#[derive(Clone, Debug, serde::Serialize)]
pub struct MerchantKeyStore {
    pub merchant_id: String,
    pub key: Encryptable<Secret<Vec<u8>>>,
    #[serde(with = "custom_serde::iso8601")]
    pub created_at: PrimitiveDateTime,
}

#[async_trait::async_trait]
impl super::behaviour::Conversion for MerchantKeyStore {
    type DstType = storage_models::merchant_key_store::MerchantKeyStore;
    type NewDstType = storage_models::merchant_key_store::MerchantKeyStoreNew;
    async fn convert(self) -> CustomResult<Self::DstType, ValidationError> {
        Ok(storage_models::merchant_key_store::MerchantKeyStore {
            key: self.key.into(),
            merchant_id: self.merchant_id,
            created_at: self.created_at,
        })
    }

    async fn convert_back(
        item: Self::DstType,
        db: &dyn StorageInterface,
        _merchant_id: &str,
        migration_timestamp: i64,
    ) -> CustomResult<Self, ValidationError>
    where
        Self: Sized,
    {
        let key = &db.get_master_key();
        Ok(Self {
            key: Encryptable::decrypt(item.key, key, GcmAes256 {}, i64::MAX, migration_timestamp)
                .await
                .change_context(ValidationError::InvalidValue {
                    message: "Failed while decrypting customer data".to_string(),
                })?,
            merchant_id: item.merchant_id,
            created_at: item.created_at,
        })
    }

    async fn construct_new(self) -> CustomResult<Self::NewDstType, ValidationError> {
        Ok(storage_models::merchant_key_store::MerchantKeyStoreNew {
            merchant_id: self.merchant_id,
            key: self.key.into(),
            created_at: date_time::now(),
        })
    }
}
