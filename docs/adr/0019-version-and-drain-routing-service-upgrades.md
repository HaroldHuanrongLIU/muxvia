# Version and drain routing service upgrades

The Control Plane and Routing Service will negotiate a versioned local RPC protocol. When replacement is required, the new release will preserve the old service until committed response streams have drained and then hand over atomically; a failed handover leaves the compatible old service running rather than interrupting active Target CLI work.
