// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

//! A module exporting runtime API implementation functions for all runtime APIs using v1
//! primitives.
//!
//! Runtimes implementing the v1 runtime API are recommended to forward directly to these
//! functions.

use sp_std::prelude::*;
use sp_std::collections::btree_map::BTreeMap;
use primitives::v1::{
	ValidatorId, ValidatorIndex, GroupRotationInfo, CoreState, ValidationData,
	Id as ParaId, OccupiedCoreAssumption, SessionIndex, ValidationCode,
	CommittedCandidateReceipt, ScheduledCore, OccupiedCore, CoreOccupied, CoreIndex,
	GroupIndex, CandidateEvent, PersistedValidationData, SessionInfo,
	InboundDownwardMessage, InboundHrmpMessage, Hash,
};
use frame_support::debug;
use crate::{initializer, inclusion, scheduler, configuration, paras, session_info, dmp, hrmp};

/// Implementation for the `validators` function of the runtime API.
pub fn validators<T: initializer::Config>() -> Vec<ValidatorId> {
	<inclusion::Module<T>>::validators()
}

/// Implementation for the `validator_groups` function of the runtime API.
pub fn validator_groups<T: initializer::Config>() -> (
	Vec<Vec<ValidatorIndex>>,
	GroupRotationInfo<T::BlockNumber>,
) {
	let groups = <scheduler::Module<T>>::validator_groups();
	let rotation_info = <scheduler::Module<T>>::group_rotation_info();

	(groups, rotation_info)
}

/// Implementation for the `availability_cores` function of the runtime API.
pub fn availability_cores<T: initializer::Config>() -> Vec<CoreState<T::Hash, T::BlockNumber>> {
	let cores = <scheduler::Module<T>>::availability_cores();
	let parachains = <paras::Module<T>>::parachains();
	let config = <configuration::Module<T>>::config();

	let rotation_info = <scheduler::Module<T>>::group_rotation_info();

	let time_out_at = |backed_in_number, availability_period| {
		let time_out_at = backed_in_number + availability_period;

		let current_window = rotation_info.last_rotation_at() + availability_period;
		let next_rotation = rotation_info.next_rotation_at();

		// If we are within `period` blocks of rotation, timeouts are being checked
		// actively. We could even time out this block.
		if time_out_at < current_window {
			time_out_at
		} else if time_out_at <= next_rotation {
			// Otherwise, it will time out at the sooner of the next rotation
			next_rotation
		} else {
			// or the scheduled time-out. This is by definition within `period` blocks
			// of `next_rotation` and is thus a valid timeout block.
			time_out_at
		}
	};

	let group_responsible_for = |backed_in_number, core_index| {
		match <scheduler::Module<T>>::group_assigned_to_core(core_index, backed_in_number) {
			Some(g) => g,
			None =>  {
				debug::warn!("Could not determine the group responsible for core extracted \
					from list of cores for some prior block in same session");

				GroupIndex(0)
			}
		}
	};

	let mut core_states: Vec<_> = cores.into_iter().enumerate().map(|(i, core)| match core {
		Some(occupied) => {
			CoreState::Occupied(match occupied {
				CoreOccupied::Parachain => {
					let para_id = parachains[i];
					let pending_availability = <inclusion::Module<T>>
						::pending_availability(para_id)
						.expect("Occupied core always has pending availability; qed");

					let backed_in_number = pending_availability.backed_in_number().clone();
					OccupiedCore {
						next_up_on_available: <scheduler::Module<T>>::next_up_on_available(
							CoreIndex(i as u32)
						),
						occupied_since: backed_in_number,
						time_out_at: time_out_at(
							backed_in_number,
							config.chain_availability_period,
						),
						next_up_on_time_out: <scheduler::Module<T>>::next_up_on_time_out(
							CoreIndex(i as u32)
						),
						availability: pending_availability.availability_votes().clone(),
						group_responsible: group_responsible_for(
							backed_in_number,
							pending_availability.core_occupied(),
						),
						candidate_hash: pending_availability.candidate_hash(),
						candidate_descriptor: pending_availability.candidate_descriptor().clone(),
					}
				}
				CoreOccupied::Parathread(p) => {
					let para_id = p.claim.0;
					let pending_availability = <inclusion::Module<T>>
						::pending_availability(para_id)
						.expect("Occupied core always has pending availability; qed");

					let backed_in_number = pending_availability.backed_in_number().clone();
					OccupiedCore {
						next_up_on_available: <scheduler::Module<T>>::next_up_on_available(
							CoreIndex(i as u32)
						),
						occupied_since: backed_in_number,
						time_out_at: time_out_at(
							backed_in_number,
							config.thread_availability_period,
						),
						next_up_on_time_out: <scheduler::Module<T>>::next_up_on_time_out(
							CoreIndex(i as u32)
						),
						availability: pending_availability.availability_votes().clone(),
						group_responsible: group_responsible_for(
							backed_in_number,
							pending_availability.core_occupied(),
						),
						candidate_hash: pending_availability.candidate_hash(),
						candidate_descriptor: pending_availability.candidate_descriptor().clone(),
					}
				}
			})
		}
		None => CoreState::Free,
	}).collect();

	// This will overwrite only `Free` cores if the scheduler module is working as intended.
	for scheduled in <scheduler::Module<T>>::scheduled() {
		core_states[scheduled.core.0 as usize] = CoreState::Scheduled(ScheduledCore {
			para_id: scheduled.para_id,
			collator: scheduled.required_collator().map(|c| c.clone()),
		});
	}

	core_states
}

fn with_assumption<Config, T, F>(
	para_id: ParaId,
	assumption: OccupiedCoreAssumption,
	build: F,
) -> Option<T> where
	Config: inclusion::Config,
	F: FnOnce() -> Option<T>,
{
	match assumption {
		OccupiedCoreAssumption::Included => {
			<inclusion::Module<Config>>::force_enact(para_id);
			build()
		}
		OccupiedCoreAssumption::TimedOut => {
			build()
		}
		OccupiedCoreAssumption::Free => {
			if <inclusion::Module<Config>>::pending_availability(para_id).is_some() {
				None
			} else {
				build()
			}
		}
	}
}

/// Implementation for the `full_validation_data` function of the runtime API.
pub fn full_validation_data<T: initializer::Config>(
	para_id: ParaId,
	assumption: OccupiedCoreAssumption,
) -> Option<ValidationData<T::BlockNumber>> {
	use parity_scale_codec::Decode as _;
	let relay_parent_number = <frame_system::Module<T>>::block_number();
	let relay_storage_root = Hash::decode(&mut &sp_io::storage::root()[..])
		.expect("storage root must decode to the Hash type; qed");
	with_assumption::<T, _, _>(para_id, assumption, || {
		Some(ValidationData {
			persisted: crate::util::make_persisted_validation_data::<T>(
				para_id,
				relay_parent_number,
				relay_storage_root,
			)?,
			transient: crate::util::make_transient_validation_data::<T>(
				para_id,
				relay_parent_number,
			)?,
		})
	})
}

/// Implementation for the `persisted_validation_data` function of the runtime API.
pub fn persisted_validation_data<T: initializer::Config>(
	para_id: ParaId,
	assumption: OccupiedCoreAssumption,
) -> Option<PersistedValidationData<T::BlockNumber>> {
	use parity_scale_codec::Decode as _;
	let relay_parent_number = <frame_system::Module<T>>::block_number();
	let relay_storage_root = Hash::decode(&mut &sp_io::storage::root()[..])
		.expect("storage root must decode to the Hash type; qed");
	with_assumption::<T, _, _>(para_id, assumption, || {
		crate::util::make_persisted_validation_data::<T>(
			para_id,
			relay_parent_number,
			relay_storage_root,
		)
	})
}

/// Implementation for the `check_validation_outputs` function of the runtime API.
pub fn check_validation_outputs<T: initializer::Config>(
	para_id: ParaId,
	outputs: primitives::v1::CandidateCommitments,
) -> bool {
	<inclusion::Module<T>>::check_validation_outputs_for_runtime_api(para_id, outputs)
}

/// Implementation for the `session_index_for_child` function of the runtime API.
pub fn session_index_for_child<T: initializer::Config>() -> SessionIndex {
	// Just returns the session index from `inclusion`. Runtime APIs follow
	// initialization so the initializer will have applied any pending session change
	// which is expected at the child of the block whose context the runtime API was invoked
	// in.
	//
	// Incidentally, this is also the rationale for why it is OK to query validators or
	// occupied cores or etc. and expect the correct response "for child".
	<inclusion::Module<T>>::session_index()
}

/// Implementation for the `validation_code` function of the runtime API.
pub fn validation_code<T: initializer::Config>(
	para_id: ParaId,
	assumption: OccupiedCoreAssumption,
) -> Option<ValidationCode> {
	with_assumption::<T, _, _>(
		para_id,
		assumption,
		|| <paras::Module<T>>::current_code(&para_id),
	)
}

/// Implementation for the `historical_validation_code` function of the runtime API.
pub fn historical_validation_code<T: initializer::Config>(
	para_id: ParaId,
	context_height: T::BlockNumber,
) -> Option<ValidationCode> {
	<paras::Module<T>>::validation_code_at(para_id, context_height, None)
}

/// Implementation for the `candidate_pending_availability` function of the runtime API.
pub fn candidate_pending_availability<T: initializer::Config>(para_id: ParaId)
	-> Option<CommittedCandidateReceipt<T::Hash>>
{
	<inclusion::Module<T>>::candidate_pending_availability(para_id)
}

/// Implementation for the `candidate_events` function of the runtime API.
// NOTE: this runs without block initialization, as it accesses events.
// this means it can run in a different session than other runtime APIs at the same block.
pub fn candidate_events<T, F>(extract_event: F) -> Vec<CandidateEvent<T::Hash>>
where
	T: initializer::Config,
	F: Fn(<T as frame_system::Config>::Event) -> Option<inclusion::Event<T>>,
{
	use inclusion::Event as RawEvent;

	<frame_system::Module<T>>::events().into_iter()
		.filter_map(|record| extract_event(record.event))
		.map(|event| match event {
			RawEvent::<T>::CandidateBacked(c, h) => CandidateEvent::CandidateBacked(c, h),
			RawEvent::<T>::CandidateIncluded(c, h) => CandidateEvent::CandidateIncluded(c, h),
			RawEvent::<T>::CandidateTimedOut(c, h) => CandidateEvent::CandidateTimedOut(c, h),
		})
		.collect()
}

/// Get the session info for the given session, if stored.
pub fn session_info<T: session_info::Config>(index: SessionIndex) -> Option<SessionInfo> {
	<session_info::Module<T>>::session_info(index)
}

/// Implementation for the `dmq_contents` function of the runtime API.
pub fn dmq_contents<T: dmp::Config>(
	recipient: ParaId,
) -> Vec<InboundDownwardMessage<T::BlockNumber>> {
	<dmp::Module<T>>::dmq_contents(recipient)
}

/// Implementation for the `inbound_hrmp_channels_contents` function of the runtime API.
pub fn inbound_hrmp_channels_contents<T: hrmp::Config>(
	recipient: ParaId,
) -> BTreeMap<ParaId, Vec<InboundHrmpMessage<T::BlockNumber>>> {
	<hrmp::Module<T>>::inbound_hrmp_channels_contents(recipient)
}
