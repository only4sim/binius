// Copyright 2025 Irreducible Inc.

use binius_field::{ExtensionField, TowerField};
use binius_math::ArithExpr;

use crate::builder::B1;

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("max_log_size must be less than or equal to F::N_BITS")]
	MaxLogSizeTooLarge,

	#[error("table size must be less than or equal to max_log_size")]
	TableSizeTooLarge,

	#[error("math error: {0}")]
	Math(#[from] binius_math::Error),
}

/// Specifications of structured columns that generated from a dynamic table size.
///
/// A structured column is one that has sufficient structure that its multilinear extension
/// can be evaluated succinctly. These are referred to as "MLE-structured" tables in [Lasso].
///
/// [Lasso]: <https://eprint.iacr.org/2023/1216>
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredDynSize {
	/// A column whose values are incrementing binary field elements in lexicographic order.
	Incrementing {
		/// The base-2 logarithm of the maximum size of the column.
		max_size_log: usize,
	},
}

impl StructuredDynSize {
	/// Returns an arithmetic expression that represents the multilinear extension of the
	/// structured column.
	pub fn expr<F: TowerField>(self) -> Result<ArithExpr<F>, Error> {
		match self {
			StructuredDynSize::Incrementing { max_size_log } => {
				incrementing_expr::<F>(max_size_log)
			}
		}
	}

	/// Returns the maximum size of the column.
	fn max_size_log(&self) -> usize {
		match self {
			StructuredDynSize::Incrementing { max_size_log } => *max_size_log,
		}
	}

	/// Checks whether the given table size specified as n_vars can fit into this structured column
	/// specifier.
	pub fn check_nvars(&self, n_vars: usize) -> Result<(), Error> {
		if n_vars > self.max_size_log() {
			Err(Error::MaxLogSizeTooLarge)
		} else {
			Ok(())
		}
	}
}

/// Returns the arithmetic expression for an incrementing column.
///
/// The multilinear expression is
///
/// $$
/// \sum_{v \in B_n} X_i \beta_i,
/// $$
///
/// where $\beta_i$ is the $i$-th basis element of the field $F$ as an $\mathbb{F}_2$ vector space.
pub fn incrementing_expr<F: TowerField>(max_log_size: usize) -> Result<ArithExpr<F>, Error> {
	if max_log_size > F::N_BITS {
		return Err(Error::MaxLogSizeTooLarge);
	}
	let expr = (0..max_log_size)
		.map(|i| ArithExpr::Var(i) * ArithExpr::Const(<F as ExtensionField<B1>>::basis(i)))
		.sum::<ArithExpr<F>>();
	Ok(expr)
}

#[cfg(test)]
mod tests {
	use std::iter::{self};

	use binius_compute::cpu::alloc::CpuComputeAllocator;
	use binius_core::polynomial::test_utils::decompose_index_to_hypercube_point;
	use binius_fast_compute::arith_circuit::ArithCircuitPoly;
	use binius_field::{BinaryField32b, arch::OptimalUnderlier128b, as_packed_field::PackedType};
	use binius_math::{ArithCircuit, CompositionPoly};
	use itertools::izip;

	use super::*;
	use crate::{
		builder::{
			B16, B32, B128, ConstraintSystem, WitnessIndex,
			test_utils::{ClosureFiller, validate_system_witness},
		},
		gadgets::structured::fill_incrementing_b32,
	};

	#[test]
	fn test_incrementing_expr() {
		let expr = incrementing_expr::<B32>(5).unwrap();
		let evaluator = ArithCircuitPoly::new(expr.into());
		for i in 0..1 << 5 {
			let bits = decompose_index_to_hypercube_point::<B32>(5, i);
			assert_eq!(evaluator.evaluate(&bits).unwrap(), B32::new(i as u32));
		}
	}

	#[test]
	fn test_fill_incrementing() {
		let mut cs = ConstraintSystem::new();
		let mut table = cs.add_table("test");
		table.require_power_of_two_size();
		let test_table_id = table.id();
		let expected_col = table.add_committed::<B32, 1>("reference");
		let structured_col = table.add_structured::<B32>(
			"incrementing",
			StructuredDynSize::Incrementing { max_size_log: 32 },
		);
		table.assert_zero("reference = structured", expected_col - structured_col);

		let mut allocator = CpuComputeAllocator::new(1 << 12);
		let allocator = allocator.into_bump_allocator();
		let mut witness =
			WitnessIndex::<PackedType<OptimalUnderlier128b, B128>>::new(&cs, &allocator);
		{
			let table_witness = witness.init_table(test_table_id, 1 << 5).unwrap();
			table_witness
				.fill_sequential_with_segment_size(
					&ClosureFiller::new(test_table_id, |events, index| {
						{
							let mut expected_col = index.get_scalars_mut::<B32, 1>(expected_col)?;
							for (&i, col_i) in iter::zip(events, &mut *expected_col) {
								*col_i = BinaryField32b::new(i);
							}
						}

						fill_incrementing_b32(index, structured_col)?;
						Ok(())
					}),
					&(0..1 << 5).collect::<Vec<_>>(),
					// Test that fill works when the segment size is less than the full index size.
					4,
				)
				.unwrap();
		}

		validate_system_witness::<OptimalUnderlier128b>(&cs, witness, vec![]);
	}

	#[test]
	fn test_fill_bitwise_and() {
		let log_size = 8;

		let mut cs = ConstraintSystem::new();
		let mut table = cs.add_table("test");
		table.require_fixed_size(log_size);
		let test_table_id = table.id();
		let expected_col = table.add_committed::<B16, 1>("reference");

		let lookup_index = (0..log_size)
			.map(|i| {
				ArithExpr::Var(i) * ArithExpr::Const(<B128 as ExtensionField<B1>>::basis(i + 4))
			})
			.sum::<ArithExpr<B128>>();

		let and_res = (0..4)
			.map(|i| {
				ArithExpr::Var(i)
					* ArithExpr::Var(4 + i)
					* ArithExpr::Const(<B128 as ExtensionField<B1>>::basis(i))
			})
			.sum::<ArithExpr<B128>>();

		let expr = lookup_index + and_res;

		let structured_col = table.add_fixed::<B16>("a|b|res", ArithCircuit::from(&expr));

		table.assert_zero("reference = structured", expected_col - structured_col);

		let mut allocator = CpuComputeAllocator::new(1 << 12);
		let allocator = allocator.into_bump_allocator();
		let mut witness =
			WitnessIndex::<PackedType<OptimalUnderlier128b, B128>>::new(&cs, &allocator);
		witness
			.fill_table_sequential(
				&ClosureFiller::new(test_table_id, |events, index| {
					{
						let mut expected_col = index.get_scalars_mut::<B16, 1>(expected_col)?;
						let mut structured_col = index.get_scalars_mut::<B16, 1>(structured_col)?;
						for (&i, col_i, s_col) in
							izip!(events, &mut *expected_col, &mut *structured_col)
						{
							let x = ((i >> 4) & 15) as u16;
							let y = (i & 15) as u16;
							let z = x & y;
							*col_i = B16::new(((i as u16) << 4) | z);
							*s_col = *col_i;
						}
					}

					Ok(())
				}),
				&(0..1 << log_size).collect::<Vec<_>>(),
			)
			.unwrap();

		validate_system_witness::<OptimalUnderlier128b>(&cs, witness, vec![]);
	}
}
