//! The local image surface: bounded preview reads, staging, and release.

use holt_rpc::{RpcError, RpcReply};

use crate::EngineService;

impl EngineService {
    // Local image surface (engine/src/images.rs): bounded preview
    // reads, pasted-image staging, and draft-chip release.
    pub(super) async fn read_image(&self, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        let params: holt_rpc::images::ImagePathParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        let path = &params.path;
        let (mime_type, data) = self.images.read(path).await.map_err(RpcError::Failed)?;
        RpcReply::value(&holt_rpc::images::ImageData { mime_type, data })
    }

    pub(super) async fn stage_image(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: holt_rpc::images::StageImageParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        let data = &params.data;
        if data.len() > (crate::images::MAX_IMAGE_BYTES as usize).div_ceil(3) * 4 {
            return Err(RpcError::BadParams(
                "Image exceeds the 25 MiB limit.".into(),
            ));
        }
        let bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data.as_bytes())
                .map_err(|error| RpcError::BadParams(format!("data is not base64: {error}")))?;
        let staged = self.images.stage(bytes).await.map_err(RpcError::Failed)?;
        RpcReply::value(&staged)
    }

    pub(super) async fn release_image(
        &self,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let params: holt_rpc::images::ImagePathParams = serde_json::from_value(params)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        let path = &params.path;
        let released = self.images.release(path).await.map_err(RpcError::Failed)?;
        RpcReply::value(&holt_rpc::images::ReleaseImageResult { released })
    }
}
