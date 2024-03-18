//! # bevy_xpbd_2d Integration for bevy-tnua
//!
//! In addition to the instruction in bevy-tnua's documentation:
//!
//! * Add [`TnuaXpbd2dPlugin`] to the Bevy app.
//! * Optionally: Add [`TnuaXpbd2dSensorShape`] to the sensor entities. This means the entity of
//!   the characters controlled by Tnua, but also other things like the entity generated by
//!   `TnuaCrouchEnforcer`, that can be affected with a closure.
use bevy::prelude::*;
use bevy_tnua_physics_integration_layer::data_for_backends::{
    TnuaGhostPlatform, TnuaGhostSensor, TnuaMotor, TnuaProximitySensor, TnuaProximitySensorOutput,
    TnuaRigidBodyTracker, TnuaToggle,
};
use bevy_tnua_physics_integration_layer::math::*;
use bevy_tnua_physics_integration_layer::subservient_sensors::TnuaSubservientSensor;
use bevy_xpbd_2d::math::{AdjustPrecision, AsF32};
use bevy_xpbd_2d::prelude::*;

use bevy_tnua_physics_integration_layer::*;

/// Add this plugin to use bevy_xpbd_2d as a physics backend.
///
/// This plugin should be used in addition to `TnuaControllerPlugin`.
pub struct TnuaXpbd2dPlugin;

impl Plugin for TnuaXpbd2dPlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            Update,
            TnuaSystemSet.run_if(|physics_time: Res<Time<Physics>>| !physics_time.is_paused()),
        );
        app.add_systems(
            Update,
            (
                update_rigid_body_trackers_system,
                update_proximity_sensors_system,
            )
                .in_set(TnuaPipelineStages::Sensors),
        );
        app.add_systems(
            Update,
            apply_motors_system.in_set(TnuaPipelineStages::Motors),
        );
    }
}

/// Add this component to make [`TnuaProximitySensor`] cast a shape instead of a ray.
#[derive(Component)]
pub struct TnuaXpbd2dSensorShape(pub Collider);

fn update_rigid_body_trackers_system(
    gravity: Res<Gravity>,
    mut query: Query<(
        &GlobalTransform,
        &LinearVelocity,
        &AngularVelocity,
        &mut TnuaRigidBodyTracker,
        Option<&TnuaToggle>,
    )>,
) {
    for (transform, linaer_velocity, angular_velocity, mut tracker, tnua_toggle) in query.iter_mut()
    {
        match tnua_toggle.copied().unwrap_or_default() {
            TnuaToggle::Disabled => continue,
            TnuaToggle::SenseOnly => {}
            TnuaToggle::Enabled => {}
        }
        let (_, rotation, translation) = transform.to_scale_rotation_translation();
        *tracker = TnuaRigidBodyTracker {
            translation: translation.adjust_precision(),
            rotation: rotation.adjust_precision(),
            velocity: linaer_velocity.0.extend(0.0),
            angvel: Vector3::new(0.0, 0.0, angular_velocity.0),
            gravity: gravity.0.extend(0.0),
        };
    }
}

#[allow(clippy::type_complexity)]
fn update_proximity_sensors_system(
    spatial_query_pipeline: Res<SpatialQueryPipeline>,
    collisions: Res<Collisions>,
    mut query: Query<(
        Entity,
        &GlobalTransform,
        &mut TnuaProximitySensor,
        Option<&TnuaXpbd2dSensorShape>,
        Option<&mut TnuaGhostSensor>,
        Option<&TnuaSubservientSensor>,
        Option<&TnuaToggle>,
    )>,
    collision_layers_entity: Query<&CollisionLayers>,
    other_object_query: Query<(
        Option<(&GlobalTransform, &LinearVelocity, &AngularVelocity)>,
        Option<&CollisionLayers>,
        Has<TnuaGhostPlatform>,
        Has<Sensor>,
    )>,
) {
    query.par_iter_mut().for_each(
        |(
            owner_entity,
            transform,
            mut sensor,
            shape,
            mut ghost_sensor,
            subservient,
            tnua_toggle,
        )| {
            match tnua_toggle.copied().unwrap_or_default() {
                TnuaToggle::Disabled => return,
                TnuaToggle::SenseOnly => {}
                TnuaToggle::Enabled => {}
            }
            let cast_origin = transform.transform_point(sensor.cast_origin.f32());
            let (_, owner_rotation, _) = transform.to_scale_rotation_translation();
            let cast_direction = owner_rotation * sensor.cast_direction;
            let cast_direction_2d = Direction2d::new(cast_direction.truncate())
                .expect("cast direction must be on the XY plane");

            struct CastResult {
                entity: Entity,
                proximity: Float,
                intersection_point: Vector2,
                // Use 3D and not 2D because converting a direction from 2D to 3D is more painful
                // than it should be.
                normal: Direction3d,
            }

            let owner_entity = if let Some(subservient) = subservient {
                subservient.owner_entity
            } else {
                owner_entity
            };

            let collision_layers = collision_layers_entity.get(owner_entity).ok();

            let mut final_sensor_output = None;
            if let Some(ghost_sensor) = ghost_sensor.as_mut() {
                ghost_sensor.0.clear();
            }
            let mut apply_cast = |cast_result: CastResult| {
                let CastResult {
                    entity,
                    proximity,
                    intersection_point,
                    normal,
                } = cast_result;

                // This fixes https://github.com/idanarye/bevy-tnua/issues/14
                if let Some(contacts) = collisions.get(owner_entity, entity) {
                    let same_order = owner_entity == contacts.entity1;
                    for manifold in contacts.manifolds.iter() {
                        if !manifold.contacts.is_empty() {
                            let manifold_normal = if same_order {
                                manifold.normal2
                            } else {
                                manifold.normal1
                            };
                            #[allow(clippy::useless_conversion)]
                            if sensor.intersection_match_prevention_cutoff
                                < manifold_normal.dot(cast_direction.truncate().into())
                            {
                                return true;
                            }
                        }
                    }
                }

                // TODO: see if https://github.com/idanarye/bevy-tnua/issues/14 replicates in XPBD,
                // and if figure out how to port its fix to XPBD.

                let Ok((
                    entity_kinematic_data,
                    entity_collision_layers,
                    entity_is_ghost,
                    entity_is_sensor,
                )) = other_object_query.get(entity)
                else {
                    return false;
                };

                let entity_linvel;
                let entity_angvel;
                if let Some((entity_transform, entity_linear_velocity, entity_angular_velocity)) =
                    entity_kinematic_data
                {
                    entity_angvel = Vector3::new(0.0, 0.0, entity_angular_velocity.0);
                    entity_linvel = entity_linear_velocity.0.extend(0.0)
                        + if 0.0 < entity_angvel.length_squared() {
                            let relative_point = intersection_point
                                - entity_transform.translation().truncate().adjust_precision();
                            // NOTE: no need to project relative_point on the
                            // rotation plane, it will not affect the cross
                            // product.
                            entity_angvel.cross(relative_point.extend(0.0))
                        } else {
                            Vector3::ZERO
                        };
                } else {
                    entity_angvel = Vector3::ZERO;
                    entity_linvel = Vector3::ZERO;
                }
                let sensor_output = TnuaProximitySensorOutput {
                    entity,
                    proximity,
                    normal,
                    entity_linvel,
                    entity_angvel,
                };

                let excluded_by_collision_layers = || {
                    let collision_layers = collision_layers.copied().unwrap_or_default();
                    let entity_collision_layers =
                        entity_collision_layers.copied().unwrap_or_default();
                    !collision_layers.interacts_with(entity_collision_layers)
                };

                if entity_is_ghost {
                    if let Some(ghost_sensor) = ghost_sensor.as_mut() {
                        ghost_sensor.0.push(sensor_output);
                    }
                    true
                } else if entity_is_sensor || excluded_by_collision_layers() {
                    true
                } else {
                    final_sensor_output = Some(sensor_output);
                    false
                }
            };

            let query_filter = SpatialQueryFilter::from_excluded_entities([owner_entity]);
            if let Some(TnuaXpbd2dSensorShape(shape)) = shape {
                let (_, _, rotation_z) = owner_rotation.to_euler(EulerRot::XYZ);
                spatial_query_pipeline.shape_hits_callback(
                    shape,
                    cast_origin.truncate().adjust_precision(),
                    rotation_z.adjust_precision(),
                    cast_direction_2d,
                    sensor.cast_range,
                    true,
                    query_filter,
                    #[allow(clippy::useless_conversion)]
                    |shape_hit_data| {
                        apply_cast(CastResult {
                            entity: shape_hit_data.entity,
                            proximity: shape_hit_data.time_of_impact,
                            intersection_point: shape_hit_data.point1,
                            normal: Direction3d::new(shape_hit_data.normal1.extend(0.0).f32())
                                .unwrap_or_else(|_| -cast_direction),
                        })
                    },
                );
            } else {
                spatial_query_pipeline.ray_hits_callback(
                    cast_origin.truncate().adjust_precision(),
                    cast_direction_2d,
                    sensor.cast_range,
                    true,
                    query_filter,
                    |ray_hit_data| {
                        apply_cast(CastResult {
                            entity: ray_hit_data.entity,
                            proximity: ray_hit_data.time_of_impact,
                            intersection_point: cast_origin.truncate().adjust_precision()
                                + ray_hit_data.time_of_impact.adjust_precision()
                                    * cast_direction_2d.adjust_precision(),
                            normal: Direction3d::new(ray_hit_data.normal.extend(0.0).f32())
                                .unwrap_or_else(|_| -cast_direction),
                        })
                    },
                );
            }
            sensor.output = final_sensor_output;
        },
    );
}

#[allow(clippy::type_complexity)]
fn apply_motors_system(
    mut query: Query<(
        &TnuaMotor,
        &mut LinearVelocity,
        &mut AngularVelocity,
        &Mass,
        &Inertia,
        &mut ExternalForce,
        &mut ExternalTorque,
        Option<&TnuaToggle>,
    )>,
) {
    for (
        motor,
        mut linare_velocity,
        mut angular_velocity,
        mass,
        inertia,
        mut external_force,
        mut external_torque,
        tnua_toggle,
    ) in query.iter_mut()
    {
        match tnua_toggle.copied().unwrap_or_default() {
            TnuaToggle::Disabled | TnuaToggle::SenseOnly => {
                *external_force = Default::default();
                return;
            }
            TnuaToggle::Enabled => {}
        }
        if motor.lin.boost.is_finite() {
            linare_velocity.0 += motor.lin.boost.truncate();
        }
        if motor.lin.acceleration.is_finite() {
            external_force.set_force(motor.lin.acceleration.truncate() * mass.0);
        }
        if motor.ang.boost.is_finite() {
            angular_velocity.0 += motor.ang.boost.z;
        }
        if motor.ang.acceleration.is_finite() {
            external_torque.set_torque(
                // NOTE: I did not actually verify that this is the correct formula. Nothing uses
                // angular acceleration yet - only angular impulses.
                inertia.0 * motor.ang.acceleration.z,
            );
        }
    }
}
