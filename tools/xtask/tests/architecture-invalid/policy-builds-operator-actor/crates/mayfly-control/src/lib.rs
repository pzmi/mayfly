use mayfly_control_domain::Actor;

pub fn promote() -> Actor {
    Actor::Operator { uid: 0 }
}
