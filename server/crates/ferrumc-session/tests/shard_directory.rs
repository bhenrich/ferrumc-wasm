use ferrumc_core::{DimensionId, GameMode, PlayerId, WorldId};
use ferrumc_math::{BlockPos, ShardPos, Vec3};
use ferrumc_session::{
    SessionError, SessionRouter, ShardCoverage, ShardDirectory, ShardDirectoryError,
};
use ferrumc_sim::{GameInput, ShardPartitioner};

const WORLD: WorldId = WorldId::new(0);
const DIMENSION: DimensionId = DimensionId::new(0);

#[test]
fn world_covering_registration_resolves_far_restored_position() {
    let mut router = SessionRouter::new();
    let home =
        ShardPartitioner::for_shard(WORLD, DIMENSION, ShardPos::new(0, 0)).expect("valid home");
    let registration = router
        .register_world_shard(home)
        .expect("world-covering registration");
    let mut inbox = registration.into_receiver();
    let player = PlayerId::offline("far-restored");
    let restored = Vec3::new(25_000_000.5, 73.0, -24_000_000.25);

    assert_ne!(
        ferrumc_session::shard_for_position(restored),
        home.position(),
        "the restored record must be far outside the registered home region",
    );

    let handle = router
        .join_player(player, "far-restored", restored)
        .expect("world coverage resolves the restored position");

    assert_eq!(handle.shard(), home.position());
    assert_eq!(router.player_shard(player), Some(home.position()));
    assert_eq!(
        inbox.try_recv(),
        Ok(GameInput::PlayerJoin {
            player,
            position: restored,
        }),
    );
}

#[test]
fn world_coverage_is_scoped_and_exact_routes_take_precedence() {
    let mut directory = ShardDirectory::new();
    let world_coverage = ShardCoverage::world(WORLD, DIMENSION);
    let world_lease = directory
        .register(world_coverage, "world")
        .expect("world registration");
    let exact_target =
        ShardPartitioner::for_block(WORLD, DIMENSION, BlockPos::new(20_000_000, 64, -20_000_000));
    let exact_coverage = ShardCoverage::exact(exact_target);
    let exact_lease = directory
        .register(exact_coverage, "exact")
        .expect("exact registration");

    assert_eq!(
        directory
            .resolve(exact_target)
            .expect("exact route")
            .endpoint(),
        &"exact",
    );
    let adjacent =
        ShardPartitioner::for_block(WORLD, DIMENSION, BlockPos::new(20_000_128, 64, -20_000_000));
    assert_eq!(
        directory
            .resolve(adjacent)
            .expect("world fallback")
            .endpoint(),
        &"world",
    );
    let other_world = ShardPartitioner::for_block(
        WorldId::new(u32::MAX),
        DIMENSION,
        BlockPos::new(20_000_000, 64, -20_000_000),
    );
    let other_dimension = ShardPartitioner::for_block(
        WORLD,
        DimensionId::new(u32::MAX),
        BlockPos::new(20_000_000, 64, -20_000_000),
    );
    assert!(directory.resolve(other_world).is_none());
    assert!(directory.resolve(other_dimension).is_none());
    directory
        .register(
            ShardCoverage::world(WorldId::new(u32::MAX), DIMENSION),
            "other-world",
        )
        .expect("maximum world id remains distinct");
    directory
        .register(
            ShardCoverage::world(WORLD, DimensionId::new(u32::MAX)),
            "other-dimension",
        )
        .expect("maximum dimension id remains distinct");
    assert_eq!(
        directory
            .resolve(other_world)
            .expect("other world route")
            .endpoint(),
        &"other-world",
    );
    assert_eq!(
        directory
            .resolve(other_dimension)
            .expect("other dimension route")
            .endpoint(),
        &"other-dimension",
    );

    assert_eq!(
        directory.remove(&exact_lease).expect("remove exact"),
        "exact",
    );
    assert_eq!(
        directory
            .resolve(exact_target)
            .expect("fallback after exact removal")
            .endpoint(),
        &"world",
    );
    assert_eq!(
        directory.remove(&world_lease).expect("remove world"),
        "world"
    );
    assert_eq!(directory.len(), 2);
}

#[test]
fn duplicate_and_stale_registration_mutations_are_atomic() {
    let mut directory = ShardDirectory::new();
    let coverage = ShardCoverage::world(WORLD, DIMENSION);
    let first = directory
        .register(coverage, "first")
        .expect("first registration");
    let duplicate = directory
        .register(coverage, "duplicate")
        .expect_err("unconditional duplicate is rejected");
    assert!(matches!(
        duplicate,
        ShardDirectoryError::RegistrationOccupied {
            coverage: occupied,
            generation,
        } if occupied == coverage && generation == first.generation()
    ));
    assert_eq!(directory.len(), 1);
    assert_eq!(
        directory
            .resolve(ShardPartitioner::for_block(
                WORLD,
                DIMENSION,
                BlockPos::new(0, 64, 0),
            ))
            .expect("original remains")
            .endpoint(),
        &"first",
    );

    let second = directory
        .replace(&first, "second")
        .expect("current lease can replace");
    assert_ne!(first.generation(), second.generation());
    assert_eq!(directory.len(), 1);

    for stale_mutation in [
        directory.replace(&first, "stale replacement").map(|_| ()),
        directory.remove(&first).map(|_| ()),
    ] {
        let error = stale_mutation.expect_err("stale lease cannot mutate");
        assert!(matches!(
            error,
            ShardDirectoryError::StaleLease {
                coverage: stale_coverage,
                lease_generation,
                current_generation: Some(current),
            } if stale_coverage == coverage
                && lease_generation == first.generation()
                && current == second.generation()
        ));
    }
    assert_eq!(
        directory
            .resolve(ShardPartitioner::for_block(
                WORLD,
                DIMENSION,
                BlockPos::new(0, 64, 0),
            ))
            .expect("replacement remains")
            .endpoint(),
        &"second",
    );
}

#[test]
fn unregister_reregister_never_reuses_a_generation() {
    let mut directory = ShardDirectory::new();
    let coverage = ShardCoverage::world(WORLD, DIMENSION);
    let first = directory
        .register(coverage, "first")
        .expect("first registration");
    assert_eq!(directory.remove(&first).expect("current removal"), "first");
    let second = directory
        .register(coverage, "second")
        .expect("registration after removal");

    assert_ne!(first.generation(), second.generation());
    let error = directory
        .remove(&first)
        .expect_err("pre-removal lease cannot ABA-match");
    assert!(matches!(
        error,
        ShardDirectoryError::StaleLease {
            current_generation: Some(current),
            ..
        } if current == second.generation()
    ));
    assert_eq!(
        directory.remove(&second).expect("new lease remains"),
        "second"
    );
}

#[test]
fn a_foreign_directory_lease_cannot_mutate_matching_coverage() {
    let coverage = ShardCoverage::world(WORLD, DIMENSION);
    let mut first_directory = ShardDirectory::new();
    let foreign = first_directory
        .register(coverage, "foreign")
        .expect("foreign registration");
    drop(first_directory);
    let mut target_directory = ShardDirectory::new();
    let target = target_directory
        .register(coverage, "target")
        .expect("target registration");

    for error in [
        target_directory.replace(&foreign, "candidate").map(|_| ()),
        target_directory.remove(&foreign).map(|_| ()),
    ] {
        assert_eq!(
            error.expect_err("foreign lease is rejected"),
            ShardDirectoryError::ForeignLease { coverage },
        );
    }
    let current = target_directory
        .current(coverage)
        .expect("target registration remains");
    assert_eq!(current.lease(), target);
    assert_eq!(current.endpoint(), &"target");
}

#[test]
fn a_lease_is_bound_to_its_coverage_key() {
    let mut directory = ShardDirectory::new();
    let world = ShardCoverage::world(WORLD, DIMENSION);
    let exact_id = ShardPartitioner::for_block(WORLD, DIMENSION, BlockPos::new(0, 64, 0));
    let exact = ShardCoverage::exact(exact_id);
    let world_lease = directory.register(world, 1_u8).expect("world");
    let exact_lease = directory.register(exact, 2_u8).expect("exact");

    assert_ne!(world_lease.coverage(), exact_lease.coverage());
    let world_next = directory
        .replace(&world_lease, 3_u8)
        .expect("replace only world");
    assert_eq!(
        directory
            .resolve(exact_id)
            .expect("exact remains")
            .endpoint(),
        &2,
    );
    assert_eq!(directory.remove(&exact_lease).expect("exact lease"), 2);
    assert_eq!(directory.remove(&world_next).expect("world lease"), 3);
}

#[test]
fn world_coverage_handles_partition_boundaries_and_extremes() {
    let mut directory = ShardDirectory::new();
    directory
        .register(ShardCoverage::world(WORLD, DIMENSION), ())
        .expect("world registration");

    for block in [
        BlockPos::new(i32::MIN, 0, i32::MIN),
        BlockPos::new(-129, 0, -129),
        BlockPos::new(-128, 0, -128),
        BlockPos::new(-1, 0, -1),
        BlockPos::new(0, 0, 0),
        BlockPos::new(127, 0, 127),
        BlockPos::new(128, 0, 128),
        BlockPos::new(i32::MAX, 0, i32::MAX),
    ] {
        let target = ShardPartitioner::for_block(WORLD, DIMENSION, block);
        assert!(
            directory.resolve(target).is_some(),
            "world coverage must resolve {block:?} through {target}",
        );
    }
}

#[test]
fn valid_endpoint_rotation_keeps_existing_player_routable() {
    let mut router = SessionRouter::new();
    let home =
        ShardPartitioner::for_shard(WORLD, DIMENSION, ShardPos::new(0, 0)).expect("valid home");
    let first = router
        .register_world_shard(home)
        .expect("world registration");
    let first_lease = first.lease();
    let mut first_inbox = first.into_receiver();
    let player = PlayerId::offline("rotation");
    let _handle = router
        .join_player(player, "rotation", Vec3::ZERO)
        .expect("join");
    assert!(matches!(
        first_inbox.try_recv(),
        Ok(GameInput::PlayerJoin { player: joined, .. }) if joined == player
    ));

    let replacement = router
        .replace_shard_registration(&first_lease)
        .expect("current lease replacement");
    assert_ne!(replacement.lease().generation(), first_lease.generation());
    let mut replacement_inbox = replacement.into_receiver();
    router
        .route_game_input(
            player,
            GameInput::SetGameMode {
                player,
                mode: GameMode::Creative,
            },
        )
        .expect("existing player follows current endpoint generation");
    assert!(matches!(
        replacement_inbox.try_recv(),
        Ok(GameInput::SetGameMode {
            player: recipient,
            mode: GameMode::Creative,
        }) if recipient == player
    ));
    assert!(first_inbox.try_recv().is_err());

    assert_eq!(
        router
            .disconnect_player(player)
            .expect("leave follows rotation"),
        home.position(),
    );
    assert_eq!(
        replacement_inbox.try_recv(),
        Ok(GameInput::PlayerLeave { player }),
    );

    let stale = router
        .replace_shard_registration(&first_lease)
        .expect_err("old lease cannot replace current registration");
    assert!(matches!(
        stale,
        SessionError::ShardDirectory(ShardDirectoryError::StaleLease { .. })
    ));
}

#[test]
fn closed_exact_endpoint_never_falls_through_to_world_coverage() {
    let mut router = SessionRouter::new();
    let world_home =
        ShardPartitioner::for_shard(WORLD, DIMENSION, ShardPos::new(0, 0)).expect("world home");
    let world = router
        .register_world_shard(world_home)
        .expect("world registration");
    let mut world_inbox = world.into_receiver();
    let exact_id = ShardPartitioner::for_block(WORLD, DIMENSION, BlockPos::new(20_000_000, 64, 0));
    let exact = router
        .register_exact_shard(exact_id)
        .expect("exact registration");
    drop(exact.into_receiver());
    let restored = Vec3::new(20_000_000.5, 64.0, 0.5);
    let player = PlayerId::offline("closed-exact");

    let error = router
        .join_player(player, "closed-exact", restored)
        .expect_err("closed exact endpoint wins resolution and fails");
    assert_eq!(
        error,
        SessionError::ShardClosed {
            shard: exact_id.position(),
        },
    );
    assert_eq!(router.player_count(), 0);
    assert!(world_inbox.try_recv().is_err());
}

#[test]
fn full_exact_endpoint_never_falls_through_to_world_coverage() {
    let mut router = SessionRouter::with_capacities(1, 16);
    let world_home =
        ShardPartitioner::for_shard(WORLD, DIMENSION, ShardPos::new(0, 0)).expect("world home");
    let world = router
        .register_world_shard(world_home)
        .expect("world registration");
    let mut world_inbox = world.into_receiver();
    let exact_id = ShardPartitioner::for_block(WORLD, DIMENSION, BlockPos::new(20_000_000, 64, 0));
    let exact = router
        .register_exact_shard(exact_id)
        .expect("exact registration");
    let _exact_inbox = exact.into_receiver();
    let player = PlayerId::offline("full-exact");
    let restored = Vec3::new(20_000_000.5, 64.0, 0.5);
    let _handle = router
        .join_player(player, "full-exact", restored)
        .expect("join fills exact endpoint");
    let input = GameInput::SetGameMode {
        player,
        mode: GameMode::Creative,
    };

    let error = router
        .route_game_input_owned(player, input.clone())
        .expect_err("full exact endpoint rejects without fallback");
    assert_eq!(error.input(), &input);
    assert_eq!(
        error.error(),
        &SessionError::ShardInboxFull {
            shard: exact_id.position(),
        },
    );
    assert!(world_inbox.try_recv().is_err());
}

#[test]
fn unregister_reregister_cannot_aba_retarget_an_existing_player() {
    let mut router = SessionRouter::new();
    let home =
        ShardPartitioner::for_shard(WORLD, DIMENSION, ShardPos::new(0, 0)).expect("world home");
    let first = router
        .register_world_shard(home)
        .expect("first registration");
    let first_lease = first.lease();
    let mut first_inbox = first.into_receiver();
    let player = PlayerId::offline("bound-before-aba");
    let _handle = router
        .join_player(player, "bound-before-aba", Vec3::ZERO)
        .expect("join");
    let _join = first_inbox.try_recv().expect("join reaches first endpoint");

    assert_eq!(
        router
            .unregister_shard_registration(&first_lease)
            .expect("remove first lineage"),
        home,
    );
    let second = router
        .register_world_shard(home)
        .expect("same coverage and home, new lineage");
    assert_ne!(
        first_lease.registration_id(),
        second.lease().registration_id(),
    );
    let mut second_inbox = second.into_receiver();

    let error = router
        .route_game_input(
            player,
            GameInput::SetGameMode {
                player,
                mode: GameMode::Creative,
            },
        )
        .expect_err("a new lineage requires explicit session handoff");
    assert!(matches!(
        error,
        SessionError::StaleShardBinding {
            home: stale_home,
            registration_id,
            current_registration_id: Some(_),
        } if stale_home == home && registration_id == first_lease.registration_id()
    ));
    assert!(router.is_player_connected(player));
    assert!(second_inbox.try_recv().is_err());

    let block_input = GameInput::BlockBreak {
        player,
        position: BlockPos::new(0, 64, 0),
        sequence: 17,
    };
    let block_error = router
        .route_game_input_owned(player, block_input.clone())
        .expect_err("block routing cannot bypass the stale home lineage");
    assert_eq!(block_error.input(), &block_input);
    assert!(matches!(
        block_error.error(),
        SessionError::StaleShardBinding {
            registration_id,
            ..
        } if *registration_id == first_lease.registration_id()
    ));
    assert!(second_inbox.try_recv().is_err());
}

#[test]
fn adding_exact_coverage_does_not_implicitly_move_an_existing_world_binding() {
    let mut router = SessionRouter::new();
    let world_home =
        ShardPartitioner::for_shard(WORLD, DIMENSION, ShardPos::new(0, 0)).expect("world home");
    let world = router
        .register_world_shard(world_home)
        .expect("world registration");
    let mut world_inbox = world.into_receiver();
    let restored = Vec3::new(20_000_000.5, 64.0, 0.5);
    let target = ShardPartitioner::for_block(WORLD, DIMENSION, BlockPos::new(20_000_000, 64, 0));
    let existing = PlayerId::offline("existing-world-binding");
    let _existing_handle = router
        .join_player(existing, "existing-world-binding", restored)
        .expect("existing world join");
    let _join = world_inbox.try_recv().expect("world join");

    let exact = router
        .register_exact_shard(target)
        .expect("more-specific coverage");
    let mut exact_inbox = exact.into_receiver();
    router
        .route_game_input(
            existing,
            GameInput::SetGameMode {
                player: existing,
                mode: GameMode::Creative,
            },
        )
        .expect("existing stable binding");
    assert!(matches!(
        world_inbox.try_recv(),
        Ok(GameInput::SetGameMode { player, .. }) if player == existing
    ));
    assert!(exact_inbox.try_recv().is_err());

    let block_input = GameInput::BlockBreak {
        player: existing,
        position: BlockPos::new(20_000_000, 64, 0),
        sequence: 23,
    };
    router
        .route_game_input_owned(existing, block_input.clone())
        .expect("a distinct exact owner receives spatial block input");
    assert_eq!(exact_inbox.try_recv(), Ok(block_input));
    assert!(world_inbox.try_recv().is_err());

    let newcomer = PlayerId::offline("new-exact-binding");
    let newcomer_handle = router
        .join_player(newcomer, "new-exact-binding", restored)
        .expect("new join takes exact precedence");
    assert_eq!(newcomer_handle.shard_id(), target);
    assert!(matches!(
        exact_inbox.try_recv(),
        Ok(GameInput::PlayerJoin { player, .. }) if player == newcomer
    ));
}

#[test]
fn legacy_exact_registration_keeps_app_adoption_boundary_explicit() {
    let mut router = SessionRouter::new();
    let _inbox = router.register_shard(ShardPos::new(0, 0));
    let restored = Vec3::new(25_000_000.5, 73.0, -24_000_000.25);
    let target = ferrumc_session::shard_for_position(restored);

    let error = router
        .join_player(PlayerId::offline("legacy-far"), "legacy-far", restored)
        .expect_err("the app must explicitly adopt world coverage in Packet 25");
    assert_eq!(error, SessionError::UnknownShard { shard: target });
}

#[test]
fn legacy_duplicate_registration_cannot_replace_the_current_endpoint() {
    let mut router = SessionRouter::new();
    let mut first_inbox = router.register_shard(ShardPos::new(0, 0));
    let player = PlayerId::offline("legacy-rotation");
    let _handle = router
        .join_player(player, "legacy-rotation", Vec3::ZERO)
        .expect("join");
    let _join = first_inbox
        .try_recv()
        .expect("first endpoint receives join");

    let mut rejected_inbox = router.register_shard(ShardPos::new(0, 0));
    router
        .route_game_input(
            player,
            GameInput::SetGameMode {
                player,
                mode: GameMode::Creative,
            },
        )
        .expect("the original endpoint remains current");

    assert_eq!(
        first_inbox.try_recv(),
        Ok(GameInput::SetGameMode {
            player,
            mode: GameMode::Creative,
        }),
    );
    assert!(rejected_inbox.try_recv().is_err());
}

#[test]
fn removed_exact_binding_cannot_fall_through_to_world_coverage() {
    let mut router = SessionRouter::new();
    let world_home =
        ShardPartitioner::for_shard(WORLD, DIMENSION, ShardPos::new(0, 0)).expect("world home");
    let world = router
        .register_world_shard(world_home)
        .expect("world registration");
    let mut world_inbox = world.into_receiver();
    let exact_id = ShardPartitioner::for_block(WORLD, DIMENSION, BlockPos::new(20_000_000, 64, 0));
    let exact = router
        .register_exact_shard(exact_id)
        .expect("exact registration");
    let exact_lease = exact.lease();
    let mut exact_inbox = exact.into_receiver();
    let player = PlayerId::offline("removed-exact");
    let restored = Vec3::new(20_000_000.5, 64.0, 0.5);
    let _handle = router
        .join_player(player, "removed-exact", restored)
        .expect("exact join");
    let _join = exact_inbox
        .try_recv()
        .expect("exact endpoint receives join");
    router
        .unregister_shard_registration(&exact_lease)
        .expect("remove exact endpoint");

    let input = GameInput::BlockBreak {
        player,
        position: BlockPos::new(20_000_000, 64, 0),
        sequence: 19,
    };
    let error = router
        .route_game_input_owned(player, input.clone())
        .expect_err("stale exact binding wins over the world fallback");
    assert_eq!(error.input(), &input);
    assert!(matches!(
        error.error(),
        SessionError::StaleShardBinding {
            registration_id,
            current_registration_id: None,
            ..
        } if *registration_id == exact_lease.registration_id()
    ));
    assert!(world_inbox.try_recv().is_err());
}
