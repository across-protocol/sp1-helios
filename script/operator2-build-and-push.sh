#!/bin/bash

# Script to build a multi-architecture Docker image and push it to Google Artifact Registry.
# MUST be run from the workspace root directory.

# --- Workspace Root Check ---
# Ensure the script is executed from the directory containing Cargo.toml, etc.
if [[ ! -f "Cargo.toml" ]] || [[ ! -d "script" ]] || [[ ! -d "program" ]] || [[ ! -d "elf" ]]; then
  echo "ERROR: This script MUST be executed from the workspace root directory" >&2
  echo "       (the directory containing Cargo.toml, script/, program/, elf/, etc.)." >&2
  echo "       Example: cd /path/to/workspace && script/operator2-build-and-push.sh" >&2
  echo "Current directory: $(pwd)" >&2
  exit 1
fi

# Exit immediately if a command exits with a non-zero status.
set -e
# Treat unset variables as an error when substituting.
# set -u # Uncomment if you want stricter variable checking
# Ensure pipelines return the status of the last command to exit non-zero.
set -o pipefail

echo "--- ZK API Docker Build & Push Script ---"
echo "INFO: Executing from workspace root: $(pwd)"
echo "INFO: Using Dockerfile: script/Dockerfile" # Clarify which Dockerfile relative to root
echo

# --- Configuration ---
GCP_PROJECT_ID="sandbox-zk-api-2223"
GCP_REGION="us-west1"            # Your Artifact Registry region
AR_REPO_NAME="zk-api-repo"        # Your Artifact Registry repository name
IMAGE_NAME="operator2"           # Your image name
# Dockerfile path relative to the WORKSPACE ROOT (build context)
DOCKERFILE_PATH="script/Dockerfile"
PLATFORMS="linux/amd64,linux/arm64" # Platforms for multi-arch build

echo "Configuration:"
echo "  Project ID: ${GCP_PROJECT_ID}"
echo "  Region:     ${GCP_REGION}"
echo "  Repository: ${AR_REPO_NAME}"
echo "  Image Name: ${IMAGE_NAME}"
echo "  Dockerfile: ${DOCKERFILE_PATH}" # Path used in build command
echo "  Platforms:  ${PLATFORMS}"
echo

# --- Get Image Tag ---
read -p "Enter the image tag [default: latest]: " USER_TAG
# Use 'latest' if input is empty, otherwise use the user's input
IMAGE_TAG=${USER_TAG:-latest}
echo "Using tag: ${IMAGE_TAG}"
echo

# --- Construct Full Image Name ---
# Format: REGION-docker.pkg.dev/PROJECT_ID/REPOSITORY_NAME/IMAGE_NAME:TAG
FULL_IMAGE_TAG="${GCP_REGION}-docker.pkg.dev/${GCP_PROJECT_ID}/${AR_REPO_NAME}/${IMAGE_NAME}:${IMAGE_TAG}"
echo "Full image name will be: ${FULL_IMAGE_TAG}"
echo

# --- Build Step ---
# Build the image first without pushing. This populates the buildx cache.
# The '.' at the end refers to the CURRENT DIRECTORY (workspace root) as the build context.
echo "Starting multi-arch image build (cache population)..."
docker buildx build \
  --platform "${PLATFORMS}" \
  -f "${DOCKERFILE_PATH}" \
  -t "${FULL_IMAGE_TAG}" \
  . # <-- This uses the CWD (workspace root) as the build context

# If the script reaches here, the build command was successful (due to set -e)
echo
echo "--- Build Successful ---"
echo "Docker image layers built successfully for tag ${IMAGE_TAG}."
echo

# --- Confirmation Step ---
read -p "Proceed to pushing to repo ${FULL_IMAGE_TAG}? (Y/N): " CONFIRM
# Convert confirmation to lowercase
CONFIRM_LC=${CONFIRM,,}

if [[ "$CONFIRM_LC" != "y" ]]; then
    echo "Push aborted by user."
    exit 1
fi

echo
echo "Proceeding with push..."
echo

# --- Push Step ---
# Run the build command again, but add --push.
# Buildx will use the cache from the previous step and perform the push.
# The '.' build context still refers to the CWD (workspace root).
docker buildx build \
  --platform "${PLATFORMS}" \
  --push \
  -f "${DOCKERFILE_PATH}" \
  -t "${FULL_IMAGE_TAG}" \
  . # <-- Build context is still the CWD

# If the script reaches here, the push command was successful
echo
echo "--- Push Successful! ---"
echo "Docker image pushed successfully:"
echo "${FULL_IMAGE_TAG}"
echo "Target Platforms: ${PLATFORMS}"

exit 0