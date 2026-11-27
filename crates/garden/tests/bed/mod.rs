//! The C7 writes of the places importer answered by an in-process garden, shared by the bed's
//! test crates.

use ulei_garden::api::Garden;
use ulei_garden::places_wire::{BedWire, CallError};
use ulei_garden::wire::c7;

/// The importer's C7 writes answered by the garden at the Unix time `.1`.
pub(crate) struct Wire<'a>(pub(crate) &'a mut Garden, pub(crate) i64);

impl BedWire for Wire<'_> {
    fn places_write(
        &mut self,
        req: &c7::PlacesWriteRequest,
    ) -> Result<c7::PlacesWriteResponse, CallError> {
        self.0
            .garden_places_write(req, self.1)
            .map_err(|e| CallError::Failed(e.to_string()))
    }

    fn places_verdict(
        &mut self,
        req: &c7::PlacesVerdictRequest,
    ) -> Result<c7::PlacesVerdictResponse, CallError> {
        self.0
            .garden_places_verdict(req, self.1)
            .map_err(|e| CallError::Failed(e.to_string()))
    }

    fn places_profile(
        &mut self,
        req: &c7::PlacesProfileRequest,
    ) -> Result<c7::PlacesProfileResponse, CallError> {
        self.0
            .garden_places_profile(req)
            .map_err(|e| CallError::Failed(e.to_string()))
    }
}
